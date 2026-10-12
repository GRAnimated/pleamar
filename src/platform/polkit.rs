//! Asking for the password when a program needs to do something as root:
//! being the session's polkit authentication agent, so that the scene draws
//! the dialog —its own, in its own look— instead of a program of somebody
//! else's putting one up, or none at all.
//!
//! `{ ready, pending, asking, message, action, icon, user, prompt, secret, info,
//!    error }`: `ready`, this process is the session's agent; `pending`, a
//! program waits to be let through, and then `message` is what polkit says is
//! wanted, `action` its name and `user` whose password it is; `asking`, an
//! answer is awaited —not while the last one is being checked—, with `prompt`
//! what is asked («Password: ») and `secret` whether the answer is to be
//! hidden; `info`, what the system says meanwhile; `error`, `none` or `wrong`
//! after an answer that was not it.
//!
//! Commands (`polkit.*`): `answer(text)`, `cancel()`.
//!
//! Who it is, is not decided here: the answer goes to polkit's own helper,
//! which is the one that asks PAM and tells polkit. This process is the
//! messenger, and never learns whether it may do anything itself.

use super::SysValue;
use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::pin::Pin;
use std::process::{Command, Stdio};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::task::{Context, Poll, Waker};
use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::{OwnedValue, Value};

const AUTHORITY: &str = "org.freedesktop.PolicyKit1";
const AUTHORITY_PATH: &str = "/org/freedesktop/PolicyKit1/Authority";
const AUTHORITY_IFACE: &str = "org.freedesktop.PolicyKit1.Authority";
const AGENT_PATH: &str = "/org/pleamar/polkit_agent";
/// Where polkit's helper waits to be connected to (polkit 126 and later).
const HELPER_SOCKET: &str = "/run/polkit/agent-helper.socket";
/// Where it is as a program of its own, which is how it was before that.
const HELPER_PROGRAMS: &[&str] = &["/usr/lib/polkit-1/polkit-agent-helper-1", "/usr/libexec/polkit-agent-helper-1", "/usr/lib/policykit-1/polkit-agent-helper-1"];
/// How many answers that were not it before giving up on a request.
const TRIES: usize = 3;

/// How a request ended, which is what polkit is answered.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Outcome {
    Granted,
    Cancelled,
    Failed,
}

/// What polkit's call waits on while somebody types: finished from another thread.
#[derive(Default)]
struct Done(Mutex<(Option<Outcome>, Option<Waker>)>);

impl Done {
    fn finish(&self, outcome: Outcome) {
        let mut d = self.0.lock().unwrap();
        d.0 = Some(outcome);
        if let Some(w) = d.1.take() {
            w.wake();
        }
    }
}

struct Wait(Arc<Done>);

impl Future for Wait {
    type Output = Outcome;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Outcome> {
        let mut d = self.0 .0.lock().unwrap();
        match d.0 {
            Some(outcome) => Poll::Ready(outcome),
            None => {
                d.1 = Some(cx.waker().clone());
                Poll::Pending
            }
        }
    }
}

struct Request {
    cookie: String,
    action: String,
    message: String,
    icon: String,
    user: String,
    done: Arc<Done>,
}

enum Answer {
    Text(String),
    Cancel,
}

/// The request being attended: what the scene is told, and how to reach whoever attends it.
struct Current {
    cookie: String,
    action: String,
    message: String,
    icon: String,
    user: String,
    prompt: String,
    secret: bool,
    asking: bool,
    info: String,
    wrong: bool,
    cancelled: bool,
    answer: Sender<Answer>,
    /// Lets go of the helper, so that whoever reads from it stops waiting.
    stop: Option<Box<dyn Fn() + Send>>,
}

#[derive(Default)]
struct State {
    ready: bool,
    now: Option<Current>,
    waiting: VecDeque<Request>,
}

static STATE: Mutex<State> = Mutex::new(State { ready: false, now: None, waiting: VecDeque::new() });
/// Whoever hears it: a scene that declares the service and its logic that
/// watches it are two, and both are told. The agent is one per process.
static LISTENERS: Mutex<Vec<Box<dyn Fn(SysValue) + Send>>> = Mutex::new(Vec::new());
/// The connection the agent lives on: polkit drops an agent whose connection goes.
static AGENT: OnceLock<Connection> = OnceLock::new();

fn value(s: &State) -> SysValue {
    let text = |t: &str| SysValue::Text(t.to_owned());
    let now = s.now.as_ref();
    let of = |f: fn(&Current) -> &str| text(now.map(f).unwrap_or_default());
    SysValue::Map(vec![
        ("ready".into(), SysValue::Bool(s.ready)),
        ("pending".into(), SysValue::Bool(now.is_some())),
        ("asking".into(), SysValue::Bool(now.is_some_and(|n| n.asking))),
        ("message".into(), of(|n| &n.message)),
        ("action".into(), of(|n| &n.action)),
        ("icon".into(), of(|n| &n.icon)),
        ("user".into(), of(|n| &n.user)),
        ("prompt".into(), of(|n| &n.prompt)),
        ("secret".into(), SysValue::Bool(now.is_none_or(|n| n.secret))),
        ("info".into(), of(|n| &n.info)),
        ("error".into(), text(if now.is_some_and(|n| n.wrong) { "wrong" } else { "none" })),
    ])
}

fn tell() {
    let v = value(&STATE.lock().unwrap());
    for f in LISTENERS.lock().unwrap().iter() {
        f(v.clone());
    }
}

/// Changes the request being attended and tells the scene.
fn change(f: impl FnOnce(&mut Current)) {
    if let Some(n) = STATE.lock().unwrap().now.as_mut() {
        f(n);
    }
    tell();
}

/// What the helper says comes written as C would write it —`g_strescape`—:
/// «Contrase\303\261a: ».
fn unescape(line: &str) -> String {
    let b = line.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'\\' || i + 1 == b.len() {
            out.push(b[i]);
            i += 1;
            continue;
        }
        i += 1;
        match b[i] {
            b'n' => out.push(b'\n'),
            b't' => out.push(b'\t'),
            b'r' => out.push(b'\r'),
            b'b' => out.push(8),
            b'f' => out.push(12),
            b'v' => out.push(11),
            b'0'..=b'7' => {
                let mut n = 0u32;
                let mut digits = 0;
                while digits < 3 && i < b.len() && (b'0'..=b'7').contains(&b[i]) {
                    n = n * 8 + (b[i] - b'0') as u32;
                    i += 1;
                    digits += 1;
                }
                out.push(n as u8);
                continue;
            }
            other => out.push(other),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// One conversation with the helper, from what it asks to what it decides:
/// `Some(true)` it was who they said, `Some(false)` it was not, `None` it was
/// left halfway —cancelled, or the helper went away—.
fn converse(from: impl Read, mut to: impl Write, answers: &Receiver<Answer>) -> Option<bool> {
    for line in BufReader::new(from).lines() {
        let line = line.ok()?;
        let (what, rest) = line.split_once(' ').unwrap_or((line.as_str(), ""));
        match what {
            "PAM_PROMPT_ECHO_OFF" | "PAM_PROMPT_ECHO_ON" => {
                let prompt = unescape(rest);
                change(|n| {
                    n.prompt = prompt;
                    n.secret = what == "PAM_PROMPT_ECHO_OFF";
                    n.asking = true;
                });
                let Ok(Answer::Text(mut answer)) = answers.recv() else { return None };
                // One line is one answer: what follows a line break would be read as the next.
                if let Some(end) = answer.find(['\n', '\r']) {
                    answer.truncate(end);
                }
                change(|n| {
                    n.asking = false;
                    n.wrong = false;
                    n.info.clear();
                });
                answer.push('\n');
                to.write_all(answer.as_bytes()).and_then(|_| to.flush()).ok()?;
            }
            "PAM_ERROR_MSG" | "PAM_TEXT_INFO" => {
                let info = unescape(rest);
                change(|n| n.info = info);
            }
            "SUCCESS" => return Some(true),
            "FAILURE" => return Some(false),
            _ => {}
        }
    }
    None
}

/// polkit's helper, reached: what it says, where to answer it, and how to let go of it.
type Helper = (Box<dyn Read + Send>, Box<dyn Write + Send>, Box<dyn Fn() + Send>);

fn helper(user: &str, cookie: &str) -> Result<Helper, String> {
    // To try this against a stand-in for polkit, whose helper is another: the
    // real one would take the stand-in's cookie for a wrong password.
    let socket = std::env::var("PLEAMAR_POLKIT_HELPER").unwrap_or_else(|_| HELPER_SOCKET.to_owned());
    if let Ok(mut stream) = UnixStream::connect(&socket) {
        // Who is to be asked, then what polkit calls this request: a line each.
        stream.write_all(format!("{user}\n{cookie}\n").as_bytes()).map_err(|e| e.to_string())?;
        let (read, shut) = (stream.try_clone().map_err(|e| e.to_string())?, stream.try_clone().map_err(|e| e.to_string())?);
        return Ok((Box::new(read), Box::new(stream), Box::new(move || drop(shut.shutdown(std::net::Shutdown::Both)))));
    }
    let program = HELPER_PROGRAMS.iter().find(|p| std::path::Path::new(p).exists()).ok_or("polkit's helper is not on this system")?;
    let mut child = Command::new(program).arg(user).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().map_err(|e| e.to_string())?;
    let (mut stdin, stdout) = (child.stdin.take().ok_or("no way to speak to the helper")?, child.stdout.take().ok_or("no way to hear the helper")?);
    stdin.write_all(format!("{cookie}\n").as_bytes()).map_err(|e| e.to_string())?;
    let child = Mutex::new(child);
    Ok((
        Box::new(stdout),
        Box::new(stdin),
        Box::new(move || {
            if let Ok(mut c) = child.lock() {
                let _ = c.kill();
                let _ = c.wait();
            }
        }),
    ))
}

/// Attends one request to its end: asks, hears the answer, and asks again
/// after one that was not it, a few times.
fn attend(r: &Request, answers: &Receiver<Answer>) -> Outcome {
    for _ in 0..TRIES {
        let (from, to, stop) = match helper(&r.user, &r.cookie) {
            Ok(h) => h,
            Err(e) => {
                eprintln!("polkit · the helper that checks the password cannot be reached ({e})");
                return Outcome::Failed;
            }
        };
        let cancelled = {
            let mut s = STATE.lock().unwrap();
            let Some(n) = s.now.as_mut() else { return Outcome::Cancelled };
            n.stop = Some(stop);
            n.cancelled
        };
        let verdict = if cancelled { None } else { converse(from, to, answers) };
        let cancelled = {
            let mut s = STATE.lock().unwrap();
            let Some(n) = s.now.as_mut() else { return Outcome::Cancelled };
            if let Some(stop) = n.stop.take() {
                stop();
            }
            n.cancelled
        };
        match verdict {
            _ if cancelled => return Outcome::Cancelled,
            Some(true) => return Outcome::Granted,
            Some(false) => change(|n| n.wrong = true),
            None => return Outcome::Failed,
        }
    }
    Outcome::Failed
}

fn current(r: &Request) -> (Current, Receiver<Answer>) {
    let (answer, answers) = channel();
    let now = Current {
        cookie: r.cookie.clone(),
        action: r.action.clone(),
        message: r.message.clone(),
        icon: r.icon.clone(),
        user: r.user.clone(),
        prompt: String::new(),
        secret: true,
        asking: false,
        info: String::new(),
        wrong: false,
        cancelled: false,
        answer,
        stop: None,
    };
    (now, answers)
}

/// A request arrives: attended now if none is, behind the others if one is.
fn take(r: Request) {
    let answers = {
        let mut s = STATE.lock().unwrap();
        if s.now.is_some() {
            s.waiting.push_back(r);
            return;
        }
        let (now, answers) = current(&r);
        s.now = Some(now);
        answers
    };
    let done = r.done.clone();
    let started = std::thread::Builder::new().name("polkit".into()).spawn(move || {
        let mut next = Some((r, answers));
        while let Some((r, answers)) = next {
            tell();
            let outcome = attend(&r, &answers);
            r.done.finish(outcome);
            let mut s = STATE.lock().unwrap();
            s.now = None;
            next = s.waiting.pop_front().map(|r| {
                let (now, answers) = current(&r);
                s.now = Some(now);
                (r, answers)
            });
            drop(s);
            tell();
        }
    });
    if started.is_err() {
        eprintln!("polkit · no thread to attend the request on");
        STATE.lock().unwrap().now = None;
        done.finish(Outcome::Failed);
    }
}

/// Gives up on a request: the one being attended, or one still waiting its turn.
fn cancel(cookie: Option<&str>) {
    let mut s = STATE.lock().unwrap();
    if let Some(cookie) = cookie {
        if let Some(at) = s.waiting.iter().position(|r| r.cookie == cookie) {
            if let Some(r) = s.waiting.remove(at) {
                r.done.finish(Outcome::Cancelled);
            }
            return;
        }
    }
    let Some(n) = s.now.as_mut().filter(|n| cookie.is_none_or(|c| c == n.cookie)) else { return };
    n.cancelled = true;
    let _ = n.answer.send(Answer::Cancel);
    if let Some(stop) = n.stop.take() {
        stop();
    }
}

/// What polkit is told of a request that did not end in a yes.
#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "org.freedesktop.PolicyKit1.Error")]
enum AgentError {
    #[zbus(error)]
    ZBus(zbus::Error),
    Failed(String),
    Cancelled(String),
}

fn name_of(uid: u32) -> Option<String> {
    let mut buffer = vec![0 as libc::c_char; 4096];
    let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
    let mut found: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: the buffer outlives the call and the name is copied out of it before it goes.
    let ok = unsafe { libc::getpwuid_r(uid, &mut entry, buffer.as_mut_ptr(), buffer.len(), &mut found) };
    if ok != 0 || found.is_null() {
        return None;
    }
    Some(unsafe { std::ffi::CStr::from_ptr(entry.pw_name) }.to_string_lossy().into_owned())
}

/// Whose password is asked, of those polkit would take: one's own if it is
/// among them —an administrator—, and otherwise the first, which is root's.
fn whose(identities: &[(String, HashMap<String, OwnedValue>)]) -> Option<String> {
    let users: Vec<u32> = identities.iter().filter(|(kind, _)| kind == "unix-user").filter_map(|(_, d)| d.get("uid").and_then(|v| u32::try_from(v).ok())).collect();
    let me = unsafe { libc::getuid() };
    let uid = if users.contains(&me) { me } else { *users.first()? };
    name_of(uid)
}

struct Agent;

#[zbus::interface(name = "org.freedesktop.PolicyKit1.AuthenticationAgent")]
impl Agent {
    /// It answers when the request has ended, however long somebody takes to type.
    async fn begin_authentication(&self, action_id: String, message: String, icon_name: String, _details: HashMap<String, String>, cookie: String, identities: Vec<(String, HashMap<String, OwnedValue>)>) -> Result<(), AgentError> {
        let Some(user) = whose(&identities) else { return Err(AgentError::Failed("nobody whose password could be asked".into())) };
        let done = Arc::new(Done::default());
        take(Request { cookie, action: action_id, message, icon: icon_name, user, done: done.clone() });
        match Wait(done).await {
            Outcome::Granted => Ok(()),
            Outcome::Cancelled => Err(AgentError::Cancelled("it was cancelled".into())),
            Outcome::Failed => Err(AgentError::Failed("it was not who they said".into())),
        }
    }

    fn cancel_authentication(&self, cookie: String) {
        cancel(Some(&cookie));
    }
}

/// The session this process is in, which is whom the agent answers for.
fn session(c: &Connection) -> Option<String> {
    if let Ok(id) = std::env::var("XDG_SESSION_ID") {
        if !id.is_empty() {
            return Some(id);
        }
    }
    let login = Proxy::new(c, "org.freedesktop.login1", "/org/freedesktop/login1", "org.freedesktop.login1.Manager").ok()?;
    let path: zbus::zvariant::OwnedObjectPath = login.call("GetSessionByPID", &(std::process::id(),)).ok()?;
    Proxy::new(c, "org.freedesktop.login1", path.as_str().to_owned(), "org.freedesktop.login1.Session").ok()?.get_property::<String>("Id").ok()
}

fn register() -> Result<Connection, String> {
    let c = zbus::blocking::connection::Builder::system().and_then(|b| b.serve_at(AGENT_PATH, Agent)).and_then(|b| b.build()).map_err(|e| e.to_string())?;
    let session = session(&c).ok_or("this process is in no login session")?;
    let subject = ("unix-session", HashMap::from([("session-id", Value::from(session))]));
    let locale = std::env::var("LANG").ok().filter(|l| !l.is_empty()).unwrap_or_else(|| "C".into());
    let authority = Proxy::new(&c, AUTHORITY, AUTHORITY_PATH, AUTHORITY_IFACE).map_err(|e| e.to_string())?;
    authority.call_method("RegisterAuthenticationAgent", &(subject, locale, AGENT_PATH)).map_err(|e| e.to_string())?;
    Ok(c)
}

/// Becomes the session's agent, if it is not yet, and tells whoever asked how things stand.
pub fn service(dispatch: Box<dyn Fn(SysValue) + Send>) -> bool {
    LISTENERS.lock().unwrap().push(dispatch);
    if AGENT.get().is_none() {
        match register() {
            Ok(c) => {
                let _ = AGENT.set(c);
                STATE.lock().unwrap().ready = true;
            }
            // Another agent has the session, or there is no polkit: the scene is told, in `ready`.
            Err(e) => eprintln!("polkit · I cannot be the one asked for the password ({e})"),
        }
    }
    tell();
    true
}

pub fn command(what: &str, args: &[SysValue]) -> Result<(), String> {
    match (what, args) {
        ("polkit.answer", [SysValue::Text(answer)]) => {
            let s = STATE.lock().unwrap();
            let n = s.now.as_ref().filter(|n| n.asking).ok_or("nothing is being asked")?;
            n.answer.send(Answer::Text(answer.clone())).map_err(|_| "nobody is waiting for that answer any more".to_owned())
        }
        ("polkit.cancel", []) => {
            cancel(None);
            Ok(())
        }
        _ => Err(format!("'{what}' is not asked like that: polkit.answer(text), polkit.cancel()")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_the_helper_says_is_read_as_it_was_written() {
        assert_eq!(unescape("Password: "), "Password: ");
        assert_eq!(unescape("Contrase\\303\\261a: "), "Contraseña: ");
        assert_eq!(unescape("a\\tb\\\\c\\\"d"), "a\tb\\c\"d");
        assert_eq!(unescape("ends in a bar\\"), "ends in a bar\\");
    }

    /// The whole of it but the bus: a request, a stand-in for the helper at the
    /// other end of a pair of sockets, and the answers a scene would give.
    #[test]
    fn a_wrong_answer_is_asked_again_and_the_right_one_ends_it() {
        let _one_at_a_time = ONE.lock().unwrap();
        let (told, heard) = channel::<String>();
        LISTENERS.lock().unwrap().push(Box::new(move |v| drop(told.send(format!("{v:?}")))));
        let request = Request { cookie: "c1".into(), action: "org.example.do".into(), message: "Wants to do it".into(), icon: String::new(), user: "ana".into(), done: Arc::default() };
        let (now, answers) = current(&request);
        STATE.lock().unwrap().now = Some(now);

        let mut answers = Some(answers);
        for (given, verdict) in [("no", "FAILURE"), ("yes", "SUCCESS")] {
            let (ours, theirs) = UnixStream::pair().unwrap();
            let stand_in = std::thread::spawn(move || {
                let mut lines = BufReader::new(theirs.try_clone().unwrap()).lines();
                let mut theirs = theirs;
                theirs.write_all(b"PAM_TEXT_INFO Place a finger\nPAM_PROMPT_ECHO_OFF Contrase\\303\\261a: \n").unwrap();
                let answer = lines.next().unwrap().unwrap();
                theirs.write_all(format!("{verdict}\n").as_bytes()).unwrap();
                answer
            });
            let mine = answers.take().unwrap();
            let talk = std::thread::spawn(move || {
                let verdict = converse(ours.try_clone().unwrap(), ours, &mine);
                (verdict, mine)
            });
            // The scene answers once it has been asked.
            loop {
                let v = heard.recv().unwrap();
                if v.contains("(\"asking\", Bool(true))") {
                    assert!(v.contains("Contraseña: "), "{v}");
                    assert!(v.contains("Wants to do it"), "{v}");
                    assert!(v.contains("Place a finger"), "{v}");
                    break;
                }
            }
            command("polkit.answer", &[SysValue::Text(format!("{given}\nwhat follows a line break is not sent"))]).unwrap();
            let (verdict_read, back) = talk.join().unwrap();
            answers = Some(back);
            assert_eq!(stand_in.join().unwrap(), given);
            assert_eq!(verdict_read, Some(verdict == "SUCCESS"));
        }
        STATE.lock().unwrap().now = None;
        LISTENERS.lock().unwrap().clear();
    }

    #[test]
    fn cancelling_lets_go_of_whoever_waits_for_an_answer() {
        let _one_at_a_time = ONE.lock().unwrap();
        let request = Request { cookie: "c2".into(), action: "org.example.do".into(), message: String::new(), icon: String::new(), user: "ana".into(), done: Arc::default() };
        let (now, answers) = current(&request);
        STATE.lock().unwrap().now = Some(now);
        let (ours, mut theirs) = UnixStream::pair().unwrap();
        theirs.write_all(b"PAM_PROMPT_ECHO_OFF Password: \n").unwrap();
        // Another request's cookie cancels nothing of this one.
        cancel(Some("another"));
        assert!(!STATE.lock().unwrap().now.as_ref().unwrap().cancelled);
        cancel(Some("c2"));
        assert_eq!(converse(ours.try_clone().unwrap(), ours, &answers), None);
        assert!(STATE.lock().unwrap().now.as_ref().unwrap().cancelled);
        STATE.lock().unwrap().now = None;
    }

    /// These share the one state a process has: one at a time.
    static ONE: Mutex<()> = Mutex::new(());
}
