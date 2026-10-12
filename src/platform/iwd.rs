//! The network, through iwd on the system bus, where there is no
//! NetworkManager: the same value and the same commands, so a scene does not
//! know which of the two it is talking to.
//!
//! `{ online, kind, name, strength, wifi, error,
//!    networks = { { ssid, strength, secure, known, active }, … } }`. Whether
//! there is a way out and through what (`online`, `kind`) is still the kernel's
//! to say: iwd knows of being associated, not of a cable nor of an address.
//!
//! Commands (`network.*`): `wifi(on)`, `scan()`, `connect(ssid[, password])`,
//! `disconnect()`, `forget(ssid)`. Connecting takes seconds —half a minute,
//! when iwd asks for the address itself—: it goes on its own thread, and how
//! it went arrives through the service: the network turns `active`, or `error`
//! stops being `none` and says why not.

use super::SysValue;
use std::collections::HashMap;
use std::sync::mpsc::{RecvTimeoutError, Sender};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use zbus::blocking::{Connection, MessageIterator, Proxy};
use zbus::zvariant::{OwnedObjectPath, OwnedValue};
use zbus::MatchRule;

const IWD: &str = "net.connman.iwd";
const DEVICE: &str = "net.connman.iwd.Device";
const STATION: &str = "net.connman.iwd.Station";
const NETWORK: &str = "net.connman.iwd.Network";
const KNOWN: &str = "net.connman.iwd.KnownNetwork";
const AGENT_MANAGER: &str = "net.connman.iwd.AgentManager";
const MANAGER_PATH: &str = "/net/connman/iwd";
const AGENT_PATH: &str = "/org/pleamar/network_agent";

type Objects = HashMap<OwnedObjectPath, HashMap<String, HashMap<String, OwnedValue>>>;

/// Why the last `connect` did not get in; empty if it did or none was asked,
/// which a scene is told as `none`.
static ERROR: Mutex<String> = Mutex::new(String::new());
/// How to tell the service something changed that iwd will not signal.
static WAKE: Mutex<Option<Sender<()>>> = Mutex::new(None);

fn objects(c: &Connection) -> Option<Objects> {
    Proxy::new(c, IWD, "/", "org.freedesktop.DBus.ObjectManager").ok()?.call("GetManagedObjects", &()).ok()
}

fn proxy<'a>(c: &Connection, path: &str, iface: &'static str) -> Option<Proxy<'a>> {
    Proxy::new(c, IWD, path.to_owned(), iface).ok()
}

fn flag(m: &HashMap<String, OwnedValue>, k: &str) -> bool {
    m.get(k).and_then(|v| bool::try_from(v).ok()).unwrap_or(false)
}

fn word(m: &HashMap<String, OwnedValue>, k: &str) -> String {
    m.get(k).and_then(|v| <&str>::try_from(v).ok()).unwrap_or_default().to_owned()
}

/// Whether iwd is there to be asked.
pub fn available() -> bool {
    Connection::system().ok().and_then(|c| objects(&c)).is_some()
}

fn failed(why: &str) {
    let changed = ERROR.lock().map(|mut e| std::mem::replace(&mut *e, why.to_owned()) != why).unwrap_or(false);
    if changed {
        if let Some(tx) = WAKE.lock().ok().and_then(|w| w.clone()) {
            let _ = tx.send(());
        }
    }
}

/// iwd gives the signal in hundredths of a dBm (`-4400`): from −100 dBm, which
/// is nothing, to −50, which is all of it, as NetworkManager counts it.
fn strength(signal: i16) -> f64 {
    let dbm = signal as f64 / 100.0;
    ((dbm + 100.0) * 2.0).clamp(0.0, 100.0) / 100.0
}

#[derive(Debug, Clone, PartialEq)]
struct Network {
    ssid: String,
    strength: f64,
    secure: bool,
    known: bool,
    active: bool,
}

/// What iwd says: whether the radio is on, and the networks in range, one per
/// name, the one in use first and then the strongest.
fn seen(c: &Connection) -> (bool, Vec<Network>) {
    let Some(o) = objects(c) else { return (false, Vec::new()) };
    let wifi = o.values().any(|i| i.get(DEVICE).is_some_and(|d| flag(d, "Powered")));
    let mut out: Vec<Network> = Vec::new();
    let mut stations: Vec<&OwnedObjectPath> = o.iter().filter(|(_, i)| i.contains_key(STATION)).map(|(p, _)| p).collect();
    stations.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    for station in stations {
        let ordered: Vec<(OwnedObjectPath, i16)> = proxy(c, station.as_str(), STATION).and_then(|p| p.call("GetOrderedNetworks", &()).ok()).unwrap_or_default();
        for (path, signal) in ordered {
            let Some(n) = o.get(&path).and_then(|i| i.get(NETWORK)) else { continue };
            let ssid = word(n, "Name");
            if ssid.is_empty() {
                continue;
            }
            let one = Network { ssid, strength: strength(signal), secure: word(n, "Type") != "open", known: n.contains_key("KnownNetwork"), active: flag(n, "Connected") };
            match out.iter_mut().find(|n| n.ssid == one.ssid) {
                Some(n) => {
                    n.strength = n.strength.max(one.strength);
                    n.secure |= one.secure;
                    n.known |= one.known;
                    n.active |= one.active;
                }
                None => out.push(one),
            }
        }
    }
    out.sort_by(|a, b| b.active.cmp(&a.active).then(b.strength.total_cmp(&a.strength)));
    out.truncate(24);
    (wifi, out)
}

/// The value a scene reads: what iwd said, with what the kernel says of the way out.
fn value(wifi: bool, networks: &[Network], route: Option<(String, bool)>) -> SysValue {
    // In tenths: the signal jitters nonstop and is not worth telling every change.
    let tenths = |s: f64| (s * 10.0).round() / 10.0;
    let (kind, mut name, mut strength) = match route {
        Some((interface, true)) => ("wifi", interface, 0.0),
        Some((interface, false)) => ("wired", interface, 1.0),
        None => ("none", String::new(), 0.0),
    };
    if kind == "wifi" {
        if let Some(n) = networks.iter().find(|n| n.active) {
            name = n.ssid.clone();
            strength = tenths(n.strength);
        }
    }
    SysValue::Map(vec![
        ("online".into(), SysValue::Bool(kind != "none")),
        ("kind".into(), SysValue::Text(kind.into())),
        ("name".into(), SysValue::Text(name)),
        ("strength".into(), SysValue::Num(strength)),
        ("wifi".into(), SysValue::Bool(wifi)),
        ("error".into(), SysValue::Text(ERROR.lock().ok().filter(|e| !e.is_empty()).map_or("none".into(), |e| e.clone()))),
        (
            "networks".into(),
            SysValue::List(
                networks
                    .iter()
                    .map(|n| {
                        SysValue::Map(vec![
                            ("ssid".into(), SysValue::Text(n.ssid.clone())),
                            ("strength".into(), SysValue::Num(tenths(n.strength))),
                            ("secure".into(), SysValue::Bool(n.secure)),
                            ("known".into(), SysValue::Bool(n.known)),
                            ("active".into(), SysValue::Bool(n.active)),
                        ])
                    })
                    .collect(),
            ),
        ),
    ])
}

/// Watches iwd, if it is there. `false` if it is not: the caller falls back to
/// the kernel alone.
pub fn service(dispatch: Box<dyn Fn(SysValue) + Send>) -> bool {
    let Ok(c) = Connection::system() else { return false };
    if objects(&c).is_none() {
        return false;
    }
    std::thread::Builder::new()
        .name("network".into())
        .spawn(move || {
            let mut last = String::new();
            let mut report = |wifi: bool, networks: &[Network]| {
                let v = value(wifi, networks, super::system::default_route());
                let fingerprint = format!("{v:?}");
                if fingerprint != last {
                    last = fingerprint;
                    dispatch(v);
                }
            };
            let (mut wifi, mut networks) = seen(&c);
            let mut asked = Instant::now();
            report(wifi, &networks);
            let (tx, rx) = std::sync::mpsc::channel::<()>();
            if let Ok(mut wake) = WAKE.lock() {
                *wake = Some(tx.clone());
            }
            // Anything iwd says: a scan done, a network joined or left, the radio.
            if let Ok(rule) = MatchRule::builder().msg_type(zbus::message::Type::Signal).sender(IWD).map(|b| b.build()) {
                let c = c.clone();
                std::thread::spawn(move || {
                    let Ok(messages) = MessageIterator::for_match_rule(rule, &c, Some(64)) else { return };
                    for _ in messages {
                        if tx.send(()).is_err() {
                            break;
                        }
                    }
                });
            }
            loop {
                // The way out is the kernel's and comes with no signal —the
                // address arrives a moment after joining—: looked at every few
                // seconds, which costs a file. iwd is asked when it speaks, and
                // now and then for the signal, of which it does not.
                match rx.recv_timeout(Duration::from_secs(3)) {
                    Ok(()) => {
                        // A scan or a connection is a burst of signals: told once, after it.
                        std::thread::sleep(Duration::from_millis(250));
                        while rx.try_recv().is_ok() {}
                        (wifi, networks) = seen(&c);
                        asked = Instant::now();
                    }
                    Err(RecvTimeoutError::Timeout) => {
                        if asked.elapsed() >= Duration::from_secs(20) {
                            (wifi, networks) = seen(&c);
                            asked = Instant::now();
                        }
                    }
                    Err(RecvTimeoutError::Disconnected) => break,
                }
                report(wifi, &networks);
            }
        })
        .is_ok()
}

/// What iwd is answered when it asks for what nobody gave: it gives up there.
#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "net.connman.iwd.Agent.Error")]
enum AgentError {
    #[zbus(error)]
    ZBus(zbus::Error),
    Canceled(String),
}

/// The one iwd asks for the password while it joins a network it does not
/// know: only while one `connect` lasts, and only with what that one was given.
struct Agent {
    password: Option<String>,
}

#[zbus::interface(name = "net.connman.iwd.Agent")]
impl Agent {
    fn release(&self) {}
    fn request_passphrase(&self, _network: OwnedObjectPath) -> Result<String, AgentError> {
        self.password.clone().ok_or_else(|| AgentError::Canceled("no password was given".into()))
    }
    fn request_private_key_passphrase(&self, _network: OwnedObjectPath) -> Result<String, AgentError> {
        Err(AgentError::Canceled("a private key is not something a scene is asked for".into()))
    }
    fn request_user_name_and_password(&self, _network: OwnedObjectPath) -> Result<(String, String), AgentError> {
        Err(AgentError::Canceled("a user name is not something a scene is asked for".into()))
    }
    fn request_user_password(&self, _network: OwnedObjectPath, _user: String) -> Result<String, AgentError> {
        self.password.clone().ok_or_else(|| AgentError::Canceled("no password was given".into()))
    }
    fn cancel(&self, _reason: String) {}
}

/// What a scene is told of a connection that did not get in, from iwd's error:
/// one word it can decide by.
fn reason(error: &str, password_given: bool) -> &'static str {
    match error.strip_prefix("net.connman.iwd.").unwrap_or(error) {
        // iwd has one answer for every way the key exchange goes wrong, and with
        // a password just typed the wrong password is by far the likeliest.
        "Failed" if password_given => "password",
        // Too short or too long to be one.
        "InvalidFormat" => "password",
        // It asked for one and there was none to give.
        "Aborted" if !password_given => "password",
        "Timeout" => "timeout",
        // Enterprise networks, which need a file written for them first.
        "NotSupported" | "NotConfigured" => "unsupported",
        "Busy" | "InProgress" => "busy",
        _ => "failed",
    }
}

fn join(path: &str, password: Option<String>) -> Result<(), zbus::Error> {
    let c = zbus::blocking::connection::Builder::system()?.serve_at(AGENT_PATH, Agent { password })?.build()?;
    let agent = OwnedObjectPath::try_from(AGENT_PATH).map_err(zbus::Error::from)?;
    // The agent is this connection's: iwd asks whoever asked it to connect.
    let manager = Proxy::new(&c, IWD, MANAGER_PATH, AGENT_MANAGER)?;
    manager.call_method("RegisterAgent", &(agent.clone(),))?;
    let joined = Proxy::new(&c, IWD, path.to_owned(), NETWORK).and_then(|n| n.call_method("Connect", &()));
    let _ = manager.call_method("UnregisterAgent", &(agent,));
    joined.map(|_| ())
}

/// The network of that name to join: the one already known, if there is one;
/// then the one a password is for, if one was given.
fn pick<'a>(o: &'a Objects, ssid: &str, password_given: bool) -> Option<(&'a OwnedObjectPath, &'a HashMap<String, OwnedValue>)> {
    let mut all: Vec<_> = o.iter().filter_map(|(p, i)| i.get(NETWORK).map(|n| (p, n))).filter(|(_, n)| word(n, "Name") == ssid).collect();
    all.sort_by(|a, b| {
        let rank = |n: &HashMap<String, OwnedValue>| (!n.contains_key("KnownNetwork"), (word(n, "Type") != "open") != password_given);
        rank(a.1).cmp(&rank(b.1)).then(a.0.as_str().cmp(b.0.as_str()))
    });
    all.into_iter().next()
}

pub fn command(what: &str, args: &[SysValue]) -> Result<(), String> {
    let c = Connection::system().map_err(|e| e.to_string())?;
    let o = objects(&c).ok_or("iwd does not answer")?;
    let err = |e: zbus::Error| e.to_string();
    let stations = || o.iter().filter(|(_, i)| i.contains_key(STATION)).filter_map(|(p, _)| proxy(&c, p.as_str(), STATION));
    match (what, args) {
        ("network.wifi", [SysValue::Bool(on)]) => {
            let mut devices = o.iter().filter(|(_, i)| i.contains_key(DEVICE)).filter_map(|(p, _)| proxy(&c, p.as_str(), DEVICE)).peekable();
            if devices.peek().is_none() {
                return Err("there is no wifi device".into());
            }
            devices.try_for_each(|d| d.set_property("Powered", *on).map_err(|e| e.to_string()))
        }
        ("network.scan", []) => {
            // One already under way is as good as this one.
            stations().for_each(|s| drop(s.call_method("Scan", &())));
            Ok(())
        }
        ("network.connect", [SysValue::Text(ssid)]) | ("network.connect", [SysValue::Text(ssid), _]) => {
            let password = match args.get(1) {
                Some(SysValue::Text(p)) if !p.is_empty() => Some(p.clone()),
                _ => None,
            };
            let (path, n) = pick(&o, ssid, password.is_some()).ok_or("that network is not in range: scan first")?;
            // iwd joins a network it knows with what it kept, and asks nobody.
            // Forgetting it here to make room for the new password would leave
            // nothing if the new one were wrong: that is for whoever asks to say.
            if password.is_some() && n.contains_key("KnownNetwork") {
                return Err("that network is known, and is joined with the password it was saved with: network.forget(ssid) first, to give it another".into());
            }
            failed("");
            let (path, ssid) = (path.to_string(), ssid.clone());
            std::thread::Builder::new()
                .name("network connect".into())
                .spawn(move || {
                    let (given, asked) = (password.is_some(), Instant::now());
                    match join(&path, password) {
                        Ok(()) => failed(""),
                        Err(e) => {
                            // Refused on the spot, for the same reason as the last
                            // time: the scene is first told there is no error, so
                            // that it sees this one arrive.
                            std::thread::sleep(Duration::from_millis(600).saturating_sub(asked.elapsed()));
                            let name = match &e {
                                zbus::Error::MethodError(name, _, _) => name.to_string(),
                                _ => String::new(),
                            };
                            // The agent is registered before asking: iwd finding none is a fault here, not a wrong password.
                            let fault = if name.ends_with(".NoAgent") { " (pleamar's fault: its agent was not registered)" } else { "" };
                            eprintln!("network · {ssid}: {e}{fault}");
                            failed(reason(&name, given));
                        }
                    }
                })
                .map(|_| ())
                .map_err(|e| e.to_string())
        }
        ("network.disconnect", []) => {
            for s in stations() {
                if let Err(e) = s.call_method("Disconnect", &()) {
                    // One that was not connected has nothing to leave.
                    if !matches!(&e, zbus::Error::MethodError(name, _, _) if name.as_str().ends_with(".NotConnected")) {
                        return Err(err(e));
                    }
                }
            }
            Ok(())
        }
        ("network.forget", [SysValue::Text(ssid)]) => {
            let mut known = o.iter().filter(|(_, i)| i.get(KNOWN).is_some_and(|k| word(k, "Name") == *ssid)).filter_map(|(p, _)| proxy(&c, p.as_str(), KNOWN)).peekable();
            if known.peek().is_none() {
                return Err("that network was never saved".into());
            }
            known.try_for_each(|k| k.call_method("Forget", &()).map(|_| ()).map_err(err))
        }
        _ => Err(format!("'{what}' is not asked like that: network.wifi(true|false), network.scan(), network.connect(ssid[, password]), network.disconnect(), network.forget(ssid)")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_signal_is_a_share_of_the_way_from_nothing_to_all() {
        assert_eq!(strength(-4400), 1.0);
        assert_eq!(strength(-5000), 1.0);
        assert_eq!(strength(-7000), 0.6);
        assert_eq!(strength(-10000), 0.0);
        assert_eq!(strength(-12000), 0.0);
    }

    #[test]
    fn a_failure_with_a_password_just_given_is_told_as_the_password() {
        assert_eq!(reason("net.connman.iwd.Failed", true), "password");
        assert_eq!(reason("net.connman.iwd.Failed", false), "failed");
        assert_eq!(reason("net.connman.iwd.InvalidFormat", true), "password");
        // It asked the agent, which had none to give.
        assert_eq!(reason("net.connman.iwd.Aborted", false), "password");
        assert_eq!(reason("net.connman.iwd.Aborted", true), "failed");
    }

    #[test]
    fn the_other_failures_are_not_mistaken_for_the_password() {
        assert_eq!(reason("net.connman.iwd.Timeout", true), "timeout");
        assert_eq!(reason("net.connman.iwd.NotConfigured", false), "unsupported");
        assert_eq!(reason("net.connman.iwd.InProgress", true), "busy");
        assert_eq!(reason("net.connman.iwd.NoAgent", true), "failed");
        assert_eq!(reason("", true), "failed");
    }

    #[test]
    fn the_way_out_is_the_kernels_and_the_name_is_iwds() {
        let nets = vec![Network { ssid: "home".into(), strength: 0.72, secure: true, known: true, active: true }];
        let get = |v: &SysValue, k: &str| match v {
            SysValue::Map(m) => m.iter().find(|(n, _)| n == k).map(|(_, v)| format!("{v:?}")).unwrap_or_default(),
            _ => String::new(),
        };
        // Joined, with the address still to come: not online yet.
        let v = value(true, &nets, None);
        assert_eq!(get(&v, "online"), format!("{:?}", SysValue::Bool(false)));
        assert_eq!(get(&v, "kind"), format!("{:?}", SysValue::Text("none".into())));
        let v = value(true, &nets, Some(("wlan0".into(), true)));
        assert_eq!(get(&v, "name"), format!("{:?}", SysValue::Text("home".into())));
        assert_eq!(get(&v, "strength"), format!("{:?}", SysValue::Num(0.7)));
        // By cable, with the wifi joined too: the cable is the way out.
        let v = value(true, &nets, Some(("enp3s0".into(), false)));
        assert_eq!(get(&v, "kind"), format!("{:?}", SysValue::Text("wired".into())));
        assert_eq!(get(&v, "name"), format!("{:?}", SysValue::Text("enp3s0".into())));
    }

    /// What this machine's iwd says, read only: `cargo test iwd -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn what_this_machines_iwd_says() {
        let c = Connection::system().expect("the system bus");
        let (wifi, networks) = seen(&c);
        println!("wifi {wifi}");
        for n in &networks {
            println!("{:>4.2} secure {:5} known {:5} active {:5} ssid of {} letters", n.strength, n.secure, n.known, n.active, n.ssid.chars().count());
        }
        println!("{:?}", value(wifi, &[], super::super::system::default_route()));
    }
}
