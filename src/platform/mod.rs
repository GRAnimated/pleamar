//! The boundary with the system. Everything that knows about Wayland —and, the
//! day it's needed, about Win32 or AppKit— lives down here; the rest of the
//! program only knows this.
//!
//! A platform has to know how to do three things:
//!  · put up the surfaces a scene asks for (`Surface`: logical size,
//!    anchor, level, exclusive zone, screens) and hand them to the render as
//!    sheets, also when a monitor arrives or leaves, or changes scale;
//!  · tell the render where the mouse is and when it clicks;
//!  · tell the system where the mouse gets in (`PlatformWindow::update_input_region`),
//!    so that what's transparent lets the click through.
//!
//! And two that are not about windows: finding an icon by its name, and the
//! **services** —what happens in the system: workspaces, active window…—, which
//! the logic asks for by a name that is the same on every system. What a
//! system doesn't have, it says it doesn't have, and the scene decides what to do without it.

use crate::scene::{Surface, ToRender};
#[cfg(not(target_os = "linux"))]
use std::sync::mpsc::Sender;

/// A piece of system data, shaped like JSON: it's what a service tells
/// the logic, which receives it as a table.
#[derive(Clone, Debug)]
pub enum SysValue {
    Null,
    Bool(bool),
    Num(f64),
    Text(String),
    List(Vec<SysValue>),
    Map(Vec<(String, SysValue)>),
}

/// Starts listening to a service. `notify` is called with the current state and
/// then every time it changes. Returns whether this system has it.
///
/// The names are the same everywhere:
///  · `workspaces` → `{ active = 3, list = { { id, name, windows, monitor }, … } }`
///  · `window`     → `{ title, class }`
///  · `apps`       → `{ { name, exec, icon }, … }`, once
///  · `audio`      → `{ volume = 0.54, muted = false }`
///  · `battery`    → `{ present, percent, charging }`
///  · `network`    → `{ online, kind = "wired" | "wifi" | "none", name, strength, wifi, networks }` (see `networkmanager`)
///  · `bluetooth`  → `{ present, powered, discovering, devices }` (see `bluez`)
///  · `media`      → `{ playing, title, artist, album, length, position, rate, art, player, players }`, or `{ player = "" }` if nothing is playing
///  · `clock`      → `{ hour, minute, second, day, month, year, weekday, time, date }`, when the minute changes
///  · `clock.seconds` → the same, every second
///  · `files:x.json` → the text of that file when it changes
///
/// And two that can only be asked, with `sys.ask`: `env` (an environment variable) and
/// `clipboard` (whatever has been copied); `clipboard.set` writes to it.
/// Who is told what a service says, and the last thing it said.
struct Hub {
    listener: Box<dyn Fn(SysValue) + Send>,
    last: Option<SysValue>,
}

/// The services already running, by who asked and which: one thread each,
/// for as long as the process lives. A logic that is read again (its file
/// saved while it runs) asks for the same ones again; it is handed the
/// running one, and told at once what it last said. Started anew each time,
/// every reload left the old logic's threads behind, still watching for
/// nobody (the notifications and the tray were the first to show it).
/// `tag` says which of its subscriptions it is (the scene's `service audio as
/// sound` and the logic's `sys.watch("audio")` are two, each told apart).
static HUBS: std::sync::Mutex<Vec<((String, String, String), std::sync::Arc<std::sync::Mutex<Hub>>)>> = std::sync::Mutex::new(Vec::new());

pub fn service(from: &str, name: &str, tag: &str, notify: Box<dyn Fn(SysValue) + Send>) -> bool {
    // (Read once and done, in a thread that ends: nothing is left behind.)
    if name == "apps" {
        return start_service(from, name, notify);
    }
    let key = (from.to_owned(), name.to_owned(), tag.to_owned());
    let mut hubs = HUBS.lock().unwrap();
    if let Some((_, hub)) = hubs.iter().find(|(k, _)| *k == key) {
        let mut h = hub.lock().unwrap();
        if let Some(v) = h.last.clone() {
            notify(v);
        }
        h.listener = notify;
        return true;
    }
    let hub = std::sync::Arc::new(std::sync::Mutex::new(Hub { listener: notify, last: None }));
    let relay = hub.clone();
    let started = start_service(
        from,
        name,
        Box::new(move |v| {
            let mut h = relay.lock().unwrap();
            h.last = Some(v.clone());
            (h.listener)(v);
        }),
    );
    if started {
        hubs.push((key, hub));
    }
    started
}

fn start_service(from: &str, name: &str, notify: Box<dyn Fn(SysValue) + Send>) -> bool {
    // The time, without calling anyone.
    if name == "clock" || name == "clock.seconds" {
        return clock::service(name, notify);
    }
    // `files:settings.json`: notifies when that file changes, also if someone else touches it.
    if let Some(which) = name.strip_prefix("files:") {
        return files::watch(from, which, notify);
    }
    #[cfg(target_os = "linux")]
    if name == "apps" {
        // Reading hundreds of files is not a matter of an instant: in its own thread.
        return std::thread::Builder::new().name("apps".into()).spawn(move || notify(desktop::applications())).is_ok();
    }
    #[cfg(target_os = "linux")]
    match name {
        // Spoken to directly; through `wpctl` only if there is no server to speak to.
        "audio" if pulse::available() => return pulse::service(notify),
        "audio" => return system::audio(notify),
        "battery" => return system::battery(notify),
        "brightness" => return system::brightness(notify),
        // NetworkManager, which can be asked things too; without it iwd, which
        // can as well; with neither, the kernel, which can only be read.
        "network" if networkmanager::available() => return networkmanager::service(notify),
        "network" if iwd::available() => return iwd::service(notify),
        "network" => return system::network(notify),
        "bluetooth" if bluez::available() => return bluez::service(notify),
        "media" => return mpris::service(notify),
        "notifications" => return notifications::service(notify),
        "polkit" => return polkit::service(notify),
        "notification_history" => return notifications::history(notify),
        "tray" => return tray::service(notify),
        "thumbnails" => return thumbnails::service(notify),
        "gamepad" => return gamepad::service(notify),
        _ => {}
    }
    // `PLEAMAR_GENERIC=1` skips the Hyprland path: it's how to test here what
    // the other compositors will see.
    #[cfg(target_os = "linux")]
    if hyprland::is_present() && std::env::var_os("PLEAMAR_GENERIC").is_none() {
        return hyprland::service(name, notify);
    }
    // Any other Wayland: the protocols everyone understands.
    #[cfg(target_os = "linux")]
    if matches!(name, "window" | "workspaces") {
        return compositor::service(name, notify);
    }
    let _ = (name, notify);
    false
}

/// Ask a service to do something: `workspaces.focus`, 3.
pub fn command(from: &str, name: &str, args: &[SysValue]) -> Result<(), String> {
    if name.starts_with("files.") {
        return files::command(from, name, args);
    }
    if let ("clipboard.set", [SysValue::Text(t)]) = (name, args) {
        clipboard_write(t);
        return Ok(());
    }
    #[cfg(target_os = "linux")]
    if let ("apps.launch", [SysValue::Text(o)]) = (name, args) {
        return desktop::launch(o);
    }
    #[cfg(target_os = "linux")]
    if name.starts_with("brightness.") {
        return system::brightness_command(name, args);
    }
    #[cfg(target_os = "linux")]
    if name.starts_with("audio.") {
        return if pulse::available() { pulse::command(name, args) } else { system::audio_command(name, args) };
    }
    #[cfg(target_os = "linux")]
    if name.starts_with("bluetooth.") {
        return bluez::command(name, args);
    }
    #[cfg(target_os = "linux")]
    if name.starts_with("polkit.") {
        return polkit::command(name, args);
    }
    #[cfg(target_os = "linux")]
    if name.starts_with("network.") {
        return if !networkmanager::available() && iwd::available() { iwd::command(name, args) } else { networkmanager::command(name, args) };
    }
    #[cfg(target_os = "linux")]
    if name.starts_with("session.") {
        return system::session_command(name, args);
    }
    #[cfg(target_os = "linux")]
    if name.starts_with("tray.") {
        return tray::command(name, args);
    }
    #[cfg(target_os = "linux")]
    if name.starts_with("notifications.") {
        return notifications::command(name, args);
    }
    #[cfg(target_os = "linux")]
    if name.starts_with("media.") {
        return mpris::command(name, args);
    }
    #[cfg(target_os = "linux")]
    if name.starts_with("thumbnails.") {
        return thumbnails::command(name, args);
    }
    #[cfg(target_os = "linux")]
    if hyprland::is_present() && std::env::var_os("PLEAMAR_GENERIC").is_none() {
        return hyprland::command(name, args);
    }
    #[cfg(target_os = "linux")]
    if name.starts_with("workspaces.") || name.starts_with("window.") {
        return compositor::command(name, args);
    }
    let _ = args;
    Err(format!("this system cannot do '{name}' yet"))
}

/// Where pleamar keeps what it has to remember from one run to the next —which
/// plugins have been approved—: each system's settings folder.
pub fn config_dir() -> std::path::PathBuf {
    let from = |v: &str| std::env::var_os(v).filter(|x| !x.is_empty()).map(std::path::PathBuf::from);
    let base = if cfg!(target_os = "windows") {
        from("APPDATA")
    } else if cfg!(target_os = "macos") {
        from("HOME").map(|h| h.join("Library/Application Support"))
    } else {
        from("XDG_CONFIG_HOME").or_else(|| from("HOME").map(|h| h.join(".config")))
    };
    base.unwrap_or_else(std::env::temp_dir).join("pleamar")
}

/// Ask a service something and wait for the answer: `tray.menu`, of whom.
/// It can take a while —there's another application on the other side—, and
/// that's why it's up to the logic, which can wait without it showing.
pub fn query(from: &str, name: &str, args: &[SysValue]) -> Result<SysValue, String> {
    if name.starts_with("files.") {
        return files::query(from, name, args);
    }
    match (name, args) {
        // The environment: what the session told this program at startup.
        ("env", [SysValue::Text(which)]) => {
            return Ok(std::env::var_os(which).map_or(SysValue::Null, |v| SysValue::Text(v.to_string_lossy().into_owned())));
        }
        ("env", _) => return Err("`env` takes the name of one variable: sys.ask(\"env\", \"HOME\")".into()),
        ("clipboard", []) => return Ok(clipboard_read().map_or(SysValue::Null, SysValue::Text)),
        // Is that the password of whoever owns the session? What a lock screen
        // needs in order to open. It is slow on purpose when it isn't.
        #[cfg(target_os = "linux")]
        ("auth.check", [SysValue::Text(password)]) => return auth::verify(password).map(SysValue::Bool),
        ("auth.check", _) => return Err("`auth.check` takes the password, and answers true or false: sys.ask(\"auth.check\", text.password)".into()),
        _ => {}
    }
    #[cfg(target_os = "linux")]
    if name.starts_with("tray.") {
        return tray::query(name, args);
    }
    let _ = args;
    Err(format!("this system cannot answer '{name}' yet"))
}

/// Opens or closes the scene's popup number `k`: a child surface of the
/// main one, at `[x, y, width, height]` inside it, that shows the scene
/// from `origin`. The sheet reaches the render like the others; if the system
/// closes it (someone clicked outside), it reports with `ToRender::PopupClosed`.
///
/// On Wayland, an `xdg_popup`. On Windows it will be a frameless window with
/// `WS_EX_NOACTIVATE`; on macOS, an `NSPanel`.
pub fn popup(k: usize, what: Option<([i32; 4], (f32, f32))>) {
    #[cfg(target_os = "linux")]
    wayland::popup(k, what);
    let _ = (k, what);
}

/// Engages or releases the session lock for surface `which`, which is
/// `kind: lock`: `Some((bounds, origin))` engages it —one surface per monitor, with
/// those bounds centred on each— and `None` releases it. Whether it is REALLY
/// engaged is said by the system, with `ToRender::LockScreen(true)`.
///
/// On Wayland it's `ext-session-lock`: the compositor guarantees it, not the drawing.
/// On Windows it will be `LockWorkStation`, which brings its own screen; on macOS, the
/// screen saver with password.
pub fn lock_screen(which: usize, what: Option<((u32, u32), (f32, f32), crate::scene::Screens)>) {
    #[cfg(target_os = "linux")]
    wayland::lock_screen(which, what);
    #[cfg(not(target_os = "linux"))]
    let _ = (which, what);
}

/// The render has let go of the sheet of surface `id`, and with it of its
/// swapchain: what the platform kept alive for it until then can go.
pub fn sheet_released(id: u32) {
    #[cfg(target_os = "linux")]
    wayland::sheet_released(id);
    #[cfg(not(target_os = "linux"))]
    let _ = id;
}

/// Sticks surface `which` to another edge, while running. On Wayland it's a
/// layer-shell request —`set_anchor` and `set_margin` work on a live
/// surface, without creating it again—; on Windows it will be moving the window and
/// repositioning its AppBar, and on macOS moving the `NSPanel`.
/// A surface changes level while running: `level: top, overlay while open`.
pub fn relayer(which: usize, level: crate::scene::Level) {
    if let Some(h) = LAYER_HOOKS.get() {
        return (h.relayer)(which, level);
    }
    #[cfg(target_os = "linux")]
    wayland::relayer(which, level);
    let _ = (which, level);
}

/// A surface keeps more or less room for itself while running: `reserve: n while …`.
/// Something dragged out of the scene (a zone with `carries:`), from the
/// press that began it: whether a drag could start.
pub fn start_drag(text: &str) -> bool {
    #[cfg(target_os = "linux")]
    if LAYER_HOOKS.get().is_none() {
        return wayland::start_drag(text);
    }
    let _ = text;
    false
}

/// What a text dragged out offers, kind by kind. Addresses —a file's path, a
/// `file://`, a link— go as `text/uri-list` (one per line), and as text too.
pub fn drag_offers(text: &str) -> Vec<(String, Vec<u8>)> {
    let lines: Vec<&str> = text.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    if lines.is_empty() {
        return Vec::new();
    }
    let is_address = |l: &str| l.starts_with('/') || l.contains("://");
    let plain = text.trim().as_bytes().to_vec();
    let mut offers = Vec::new();
    if lines.iter().all(|l| is_address(l)) {
        let uris: String = lines.iter().map(|l| if l.starts_with('/') { format!("file://{}\r\n", encode_path(l)) } else { format!("{l}\r\n") }).collect();
        offers.push(("text/uri-list".to_owned(), uris.into_bytes()));
    }
    for kind in ["text/plain;charset=utf-8", "UTF8_STRING", "text/plain"] {
        offers.push((kind.to_owned(), plain.clone()));
    }
    offers
}

/// A path as an address: what is not a plain letter, digit or `/-._~` goes as `%XX`.
fn encode_path(path: &str) -> String {
    path.bytes().map(|b| if b.is_ascii_alphanumeric() || b"/-._~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}

pub fn rezone(which: usize, zone: i32) {
    // A platform that places surfaces itself (pleamar-wm's session) keeps none.
    if LAYER_HOOKS.get().is_some() {
        return;
    }
    #[cfg(target_os = "linux")]
    wayland::rezone(which, zone);
    let _ = (which, zone);
}

/// How many layers have been put on a monitor so far. A monitor that comes back
/// (after a deep sleep) gets its layers new, with the scene's defaults: when this
/// number moves, the render sends again what each surface has changed (edge, level,
/// room), or the new ones would sit where the scene began.
pub fn layers_placed() -> u64 {
    #[cfg(target_os = "linux")]
    return wayland::layers_placed();
    #[cfg(not(target_os = "linux"))]
    0
}

pub fn reanchor(which: usize, anchor: crate::scene::SurfaceAnchor) {
    if let Some(h) = LAYER_HOOKS.get() {
        return (h.reanchor)(which, anchor);
    }
    #[cfg(target_os = "linux")]
    wayland::reanchor(which, anchor);
    let _ = (which, anchor);
}

/// What a platform handed over does when a surface changes level or edge
/// while running (`level: top, overlay while …`, an anchor from a fact).
pub struct LayerHooks {
    pub relayer: Box<dyn Fn(usize, crate::scene::Level) + Send + Sync>,
    pub reanchor: Box<dyn Fn(usize, crate::scene::SurfaceAnchor) + Send + Sync>,
}

static LAYER_HOOKS: std::sync::OnceLock<LayerHooks> = std::sync::OnceLock::new();

pub fn provide_layer_hooks(h: LayerHooks) {
    let _ = LAYER_HOOKS.set(h);
}

/// That a process we launch does not outlive us, not even if we're killed
/// forcibly. On Linux we ask the kernel; on Windows it will be a Job Object.
pub fn die_with_parent(command: &mut std::process::Command) {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::process::CommandExt;
        // It only calls `prctl`, which is one of those that can be used between `fork` and `exec`.
        unsafe {
            command.pre_exec(|| {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                Ok(())
            });
        }
    }
    let _ = command;
}

/// A scene whose input is not a terminal (started by a session, its input
/// /dev/null) still has the one it was started from as its controlling
/// terminal, and so does every program it starts: one that asks something there
/// (`sudo` wanting a password) is stopped by the kernel for good, and whatever
/// waits for it with it. Let go of it: such a program finds no terminal and
/// fails at once. Not a session's leader (it would hang up its terminal), nor
/// a scene typed into a terminal (Ctrl+C there still reaches it).
pub fn leave_terminal() {
    #[cfg(target_os = "linux")]
    unsafe {
        if libc::isatty(0) == 1 || libc::getsid(0) == libc::getpid() {
            return;
        }
        let fd = libc::open(c"/dev/tty".as_ptr(), libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC);
        if fd >= 0 {
            libc::ioctl(fd, libc::TIOCNOTTY);
            libc::close(fd);
        }
    }
}

/// A process group of its own: a program that reads the terminal the scene may
/// have (`sudo` wanting a password) is stopped by the kernel alone, not with
/// the whole group it would share — the scene too. (A scene that does not use
/// its terminal lets go of it at the start, `leave_terminal`: there such a
/// program finds none, and fails at once.)
pub fn apart(command: &mut std::process::Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // A process group of its own, not a session: `pre_exec` (what `setsid` needs) makes Rust fork the whole scene instead of
        // `posix_spawn`ing, and forking a scene that holds a gigabyte stalls every thread in it, the painting one too.
        command.process_group(0);
    }
    let _ = command;
}

/// What answers each line said to the scene. It returns the answer, if any;
/// one that goes on talking (`watch`) writes its lines with `out`, which says
/// `false` once whoever asked has gone.
pub type Commands = std::sync::Arc<dyn Fn(String, &mut dyn FnMut(&str) -> bool) -> Option<String> + Send + Sync>;

/// Commands from outside: a global compositor shortcut, a script, another
/// application. One line of text per command, through a socket named after the
/// scene. `pleamar --say "emit toggle"` is the other end.
///
/// On Linux and macOS, a Unix socket. On Windows it will be a named pipe.
#[cfg(unix)]
pub fn listen_for_commands(scene: &str, receive: Commands) {
    use std::io::{BufRead, BufReader, Write};
    let Some(mut path) = command_socket_path(scene) else { return };
    let _ = std::fs::create_dir_all(path.parent().unwrap());
    // If there's already ANOTHER scene with this name listening, the socket is its.
    // It used to be deleted without looking: the newcomer took it, and when it left
    // it left the first one deaf forever —a ten-second trial run took the
    // keyboard shortcuts away from the everyday bar—. Only the socket of
    // someone no longer there is deleted; if they are there, this one answers to its name and its pid.
    if std::os::unix::net::UnixStream::connect(&path).is_ok() {
        let own = format!("{scene}-{}", std::process::id());
        eprintln!("orders · another '{scene}' is already listening: this one answers to '{own}' (pleamar --say {own} …)");
        let Some(other) = command_socket_path(&own) else { return };
        path = other;
    }
    let _ = std::fs::remove_file(&path);
    let Ok(listener) = std::os::unix::net::UnixListener::bind(&path) else { return };
    std::thread::Builder::new()
        .name("commands".into())
        .spawn(move || {
            for mut c in listener.incoming().map_while(Result::ok) {
                let Ok(reader) = c.try_clone() else { continue };
                // Each one on its own: a `wait` or a `watch` that lasts does not
                // keep the next order waiting (an agent presses while it watches).
                let receive = receive.clone();
                let _ = std::thread::Builder::new().name("command".into()).spawn(move || {
                    for line in BufReader::new(reader).lines().map_while(Result::ok) {
                        let mut out = |l: &str| writeln!(c, "{l}").and_then(|_| c.flush()).is_ok();
                        // A question (`get open`) is answered back the way it came.
                        if let Some(answer) = receive(line, &mut out) {
                            let _ = writeln!(c, "{}", answer.trim_end_matches('\n'));
                        }
                    }
                });
            }
        })
        .ok();
}

#[cfg(unix)]
/// Where a scene listens: `$XDG_RUNTIME_DIR/pleamar`, or `PLEAMAR_SOCKETS` —a
/// desktop of its own beside another of the same user (pleamar-wm's session
/// next to Hyprland) gives its programs another place, so that `marea
/// search` there reaches its own Marea and not the other desktop's—.
fn command_socket_path(scene: &str) -> Option<std::path::PathBuf> {
    let dir = match std::env::var("PLEAMAR_SOCKETS").ok().filter(|d| !d.is_empty()) {
        Some(d) => std::path::PathBuf::from(d),
        None => std::path::Path::new(&std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| std::env::temp_dir().to_string_lossy().into_owned())).join("pleamar"),
    };
    Some(dir.join(format!("{scene}.sock")))
}

/// The scenes running where `--say` reaches: those whose socket answers.
#[cfg(unix)]
pub fn running_scenes() -> Vec<String> {
    let Some(dir) = command_socket_path("x").and_then(|r| r.parent().map(|p| p.to_owned())) else { return Vec::new() };
    let mut alive: Vec<String> = std::fs::read_dir(&dir)
        .map(|d| d.filter_map(Result::ok).map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "sock") && std::os::unix::net::UnixStream::connect(p).is_ok()).filter_map(|p| p.file_stem().map(|n| n.to_string_lossy().into_owned())).collect())
        .unwrap_or_default();
    alive.sort();
    // pleamar-wm leaves its scene there under two names (`wm`, and its own):
    // one is a link to the other, and it would be measured twice.
    let real = |n: &str| std::fs::canonicalize(dir.join(format!("{n}.sock"))).ok();
    let mut seen = Vec::new();
    alive.retain(|n| {
        let r = real(n);
        if seen.contains(&r) {
            return false;
        }
        seen.push(r);
        true
    });
    alive
}
#[cfg(not(unix))]
pub fn running_scenes() -> Vec<String> {
    Vec::new()
}

/// Tell a running scene something and bring back what it answers.
#[cfg(unix)]
pub fn ask(scene: &str, command: &str, wait: std::time::Duration) -> Result<String, String> {
    use std::io::Write;
    let path = command_socket_path(scene).ok_or("I don't know where the sockets are")?;
    let mut s = std::os::unix::net::UnixStream::connect(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    writeln!(s, "{command}").map_err(|e| e.to_string())?;
    let _ = s.shutdown(std::net::Shutdown::Write);
    let _ = s.set_read_timeout(Some(wait));
    let mut answer = String::new();
    let _ = std::io::Read::read_to_string(&mut s, &mut answer);
    Ok(answer)
}
#[cfg(not(unix))]
pub fn ask(_: &str, _: &str, _: std::time::Duration) -> Result<String, String> {
    Err("this system has nowhere to receive commands yet".into())
}

/// Tell a running scene something: each command a line, on one connection, so
/// they are run in order. Without a name, to the only one there is.
#[cfg(unix)]
pub fn send(scene: Option<&str>, commands: &[String]) -> Result<(), String> {
    use std::io::Write;
    let path = match scene {
        Some(e) => command_socket_path(e).ok_or("I don't know where the sockets are")?,
        None => {
            let dir = command_socket_path("x").and_then(|r| r.parent().map(|p| p.to_owned())).ok_or("I don't know where the sockets are")?;
            let alive: Vec<_> = std::fs::read_dir(&dir).map_err(|_| "there is no scene running")?.filter_map(Result::ok).map(|e| e.path()).filter(|p| std::os::unix::net::UnixStream::connect(p).is_ok()).collect();
            match alive.as_slice() {
                [one] => one.clone(),
                [] => return Err("there is no scene running".into()),
                several => return Err(format!("there are several scenes running; say which: {}", several.iter().filter_map(|p| p.file_stem()).map(|n| n.to_string_lossy()).collect::<Vec<_>>().join(", "))),
            }
        }
    };
    let mut s = std::os::unix::net::UnixStream::connect(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    for command in commands {
        writeln!(s, "{command}").map_err(|e| e.to_string())?;
    }
    // Everything has been said: if it was a question, now comes whatever they answer.
    let _ = s.shutdown(std::net::Shutdown::Write);
    // A `wait` waits up to a minute; a `watch` talks for as long as it was asked.
    let _ = s.set_read_timeout(if commands.iter().any(|c| c.starts_with("watch")) { None } else { Some(std::time::Duration::from_secs(65)) });
    // Line by line, as it comes: a `watch` is read while it happens.
    for line in std::io::BufRead::lines(std::io::BufReader::new(s)).map_while(Result::ok) {
        println!("{line}");
        let _ = std::io::stdout().flush();
    }
    Ok(())
}

#[cfg(not(unix))]
pub fn listen_for_commands(_: &str, _: Commands) {}
#[cfg(not(unix))]
pub fn send(_: Option<&str>, _: &[String]) -> Result<(), String> {
    Err("this system has nowhere to receive commands yet: the named pipe is missing".into())
}

/// What became of the agent's cursor sent to a point (see `agent_cursor_to`).
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum CursorTrip {
    Going,
    Arrived,
    /// The user stopped the agent (pleamar-wm's «Stop»): nothing more is done.
    Stopped,
    /// There is no agent's cursor here (no pleamar-wm, or `agent on` missing).
    Nowhere,
}

/// Where the agent's cursor is sent: this program's window, in the pixels of
/// its picture; or one of its surfaces that is no window (a panel), of that
/// size, in its units.
#[derive(Clone, Copy, Debug)]
pub enum CursorOn {
    Window,
    Panel(u32, u32),
}

/// The agent's own cursor of the session (pleamar-wm, `cua-inject v1`), glided
/// to (x, y) of this program's window or panel: whoever
/// watches sees it reach what it is about to press before it is pressed. Fast,
/// as a decided hand: 60 to 180 ms by how far it goes; `glide: false`, at once
/// (the steps of a drag, which already come one by one).
#[cfg(unix)]
pub fn agent_cursor_to(on: CursorOn, x: f32, y: f32, glide: bool) -> std::sync::Arc<std::sync::Mutex<CursorTrip>> {
    use std::io::{BufRead, BufReader, Write};
    let trip = std::sync::Arc::new(std::sync::Mutex::new(CursorTrip::Going));
    let set = trip.clone();
    let path = std::env::var("CUA_INJECT_SOCKET").ok().filter(|p| !p.is_empty());
    std::thread::spawn(move || {
        let end = |t| *set.lock().unwrap() = t;
        let Some(stream) = path.and_then(|p| std::os::unix::net::UnixStream::connect(p).ok()) else { return end(CursorTrip::Nowhere) };
        let _ = stream.set_read_timeout(Some(std::time::Duration::from_millis(400)));
        let (Ok(mut w), mut r) = (stream.try_clone(), BufReader::new(stream)) else { return end(CursorTrip::Nowhere) };
        let mut say = |l: &str| -> Option<String> {
            writeln!(w, "{l}").ok()?;
            let mut reply = String::new();
            r.read_line(&mut reply).ok()?;
            Some(reply.trim_end().to_owned())
        };
        if say("cua-inject v1").is_none() {
            return end(CursorTrip::Nowhere);
        }
        let me = std::process::id();
        // The same hands either way: on a window by its process, on a panel by its size too.
        let to = |px: f32, py: f32| match on {
            CursorOn::Window => format!("m root:{me} 0 {px:.1} {py:.1}"),
            CursorOn::Panel(w, h) => format!("M {me} {w} {h} 0 {px:.1} {py:.1}"),
        };
        if !glide {
            let reply = say(&to(x, y));
            return end(if reply.as_deref() == Some("err stopped-by-user") { CursorTrip::Stopped } else { CursorTrip::Arrived });
        }
        let nums = |r: Option<String>, head: &str| -> Option<Vec<f32>> { r?.strip_prefix(head).map(|v| v.split_whitespace().filter_map(|n| n.parse().ok()).collect()) };
        let (at, rect) = match on {
            CursorOn::Window => (nums(say("p 0"), "at "), nums(say(&format!("r {me}")), "rect ")),
            CursorOn::Panel(..) => (None, None),
        };
        // From where it is if that is on this window; else from a little before.
        let from = match (at.filter(|a| a.len() >= 2), rect.filter(|r| r.len() >= 4)) {
            (Some(a), Some(r)) if a[0] >= r[0] && a[1] >= r[1] && a[0] < r[0] + r[2] && a[1] < r[1] + r[3] => (a[0] - r[0], a[1] - r[1]),
            _ => (x - 40.0, y - 26.0),
        };
        let far = (x - from.0).hypot(y - from.1);
        let total = (60.0 + far * 0.15).clamp(60.0, 180.0);
        let steps = if far < 2.0 { 1 } else { ((total / 12.0).round() as u32).max(3) };
        for k in 1..=steps {
            let t = k as f32 / steps as f32;
            let e = t * t * (3.0 - 2.0 * t);
            let reply = say(&to(from.0 + (x - from.0) * e, from.1 + (y - from.1) * e));
            if reply.as_deref() == Some("err stopped-by-user") {
                return end(CursorTrip::Stopped);
            }
            if k < steps {
                std::thread::sleep(std::time::Duration::from_millis((total / steps as f32) as u64));
            }
        }
        end(CursorTrip::Arrived)
    });
    trip
}

#[cfg(not(unix))]
pub fn agent_cursor_to(_: CursorOn, _: f32, _: f32, _: bool) -> std::sync::Arc<std::sync::Mutex<CursorTrip>> {
    std::sync::Arc::new(std::sync::Mutex::new(CursorTrip::Nowhere))
}

/// The system clipboard. `arboard` speaks it on all three systems; it lives
/// here so the core keeps knowing nothing about any of them.
pub fn clipboard_read() -> Option<String> {
    arboard::Clipboard::new().ok()?.get_text().ok()
}

pub fn clipboard_write(text: &str) {
    // On Wayland whoever copies has to stay alive to serve what was copied: a
    // thread that holds on to it until someone else copies something else.
    let text = text.to_owned();
    std::thread::spawn(move || {
        if let Ok(mut c) = arboard::Clipboard::new() {
            #[cfg(target_os = "linux")]
            {
                use arboard::SetExtLinux;
                let _ = c.set().wait().text(text);
            }
            #[cfg(not(target_os = "linux"))]
            let _ = c.set_text(text);
        }
    });
}

/// Give back to the system the memory that was used to read a scene. Compiling
/// Marea (3 700 instructions) goes through 128 MB for a moment to settle at 9,
/// and glibc keeps what was freed in each thread's arenas: the process
/// stayed at 355 MB of RSS forever. It's done a while later, when the
/// workshop and the logic have already done their thing with it. On other systems, or with
/// another libc, it's not needed or we don't know how to ask: it does nothing.
pub fn release_memory() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    std::thread::Builder::new()
        .name("release".into())
        .spawn(|| {
            std::thread::sleep(std::time::Duration::from_secs(3));
            // It only gives back free pages: it touches nothing alive.
            unsafe { libc::malloc_trim(0) };
        })
        .ok();
}

/// What the render asks of a system window.
pub trait PlatformWindow: Send {
    /// Where the mouse gets in: only through these boxes, in logical pixels.
    fn update_input_region(&self, boxes: &[[i32; 4]]);
    /// Which cursor shows while the mouse is over it.
    fn cursor(&self, c: crate::scene::Cursor);
    /// Grab or release the keyboard with the scene running: a launcher wants it
    /// entirely while it's open, and not at all when it isn't.
    fn keyboard(&self, t: crate::scene::Keyboard);
    /// That the system report (`ToRender::Frame`) when it wants the frame after
    /// the one about to be presented. Where that's unknown, it doesn't report, and the clock sets the pace.
    fn request_frame(&self) {}
    /// What the system blurs behind the window: rectangles in logical
    /// pixels, those of its glass. Empty, nothing. Where it can't be asked for, it does nothing.
    fn update_blur_region(&self, _boxes: &[[i32; 4]]) {}
    /// Ask for what's shown on screen in that box of the window —x, y, width and
    /// height, logical—, **with the window on top**: it arrives as `ToRender::Backdrop`. It's
    /// what lets you see what's behind the glass: what the window itself painted
    /// gets subtracted from it. With `on_change`, it doesn't arrive until something changes in that
    /// box; without it, the next time the compositor paints the screen.
    /// `false` if it can't be asked for, or if there's already one on the way.
    fn capture_backdrop(&self, _bounds: [i32; 4], _on_change: bool) -> bool {
        false
    }
    /// Forget the capture that's on the way: it won't arrive anymore.
    fn cancel_backdrop(&self) {}
    /// On which monitor it is, by name, and where on it: its top left corner,
    /// logical. `None` where that is not known.
    fn desktop_place(&self) -> Option<(String, (i32, i32))> {
        None
    }
}

/// A capture of what was seen on screen in a box of a window.
pub struct Backdrop {
    /// Which window it belongs to.
    pub sheet: u32,
    /// In real pixels, four bytes per pixel: blue, green, red and alpha.
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    /// With what opacity the compositor blended ours: 1 except in a
    /// window whose opacity is lowered (Hyprland: `decoration:active_opacity`).
    pub opacity: f32,
    /// The pixels, where the compositor left them, without copying them. `None` if the capture failed.
    pub data: Option<std::sync::Arc<dyn AsRef<[u8]> + Send + Sync>>,
}

/// Whether the scene running wants to know where the mouse is on the whole
/// desktop (`cursor.x`): the render says so every time a scene arrives.
pub static CURSOR_WANTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Where the mouse is when it is not over us. Wayland does not tell a surface
/// that —on purpose—, so it is asked of whoever knows: Hyprland, through its
/// socket, about thirty times a second; pleamar-wm, through the `cursor.sock`
/// of its session, which says it as it moves. Only while a scene wants it. It
/// reaches the render as `ToRender::Cursor` when it moves. Elsewhere nothing
/// arrives, and `cursor.x` is only known over the scene, like `pointer.x`.
pub fn watch_cursor(to_render: std::sync::mpsc::Sender<crate::scene::ToRender>) {
    #[cfg(target_os = "linux")]
    if hyprland::is_present() && std::env::var_os("PLEAMAR_GENERIC").is_none() {
        hyprland::watch_cursor(to_render);
    } else if let Some(dir) = std::env::var("PLEAMAR_SOCKETS").ok().filter(|d| std::path::Path::new(&format!("{d}/cursor.sock")).exists()) {
        session_cursor(dir, to_render);
    }
    #[cfg(not(target_os = "linux"))]
    let _ = to_render;
}

/// pleamar-wm's `cursor.sock`: a line, `x y` in units of the desktop, each
/// time the mouse moves. Connected only while a scene wants it, and let go of
/// when it no longer does; the monitors, to place it, from the `desktop` file
/// beside it (`monitor name width height x y mhz focused scale`, the size in
/// pixels), read again every two seconds.
#[cfg(target_os = "linux")]
fn session_cursor(dir: String, to_render: std::sync::mpsc::Sender<crate::scene::ToRender>) {
    use std::io::BufRead;
    use std::sync::atomic::Ordering;
    let monitors_of = |dir: &str| -> Vec<(String, [i32; 4])> {
        let text = std::fs::read_to_string(format!("{dir}/desktop")).unwrap_or_default();
        text.lines()
            .filter_map(|l| l.strip_prefix("monitor "))
            .filter_map(|l| {
                let f: Vec<&str> = l.split(' ').collect();
                let [name, w, h, x, y, _mhz, _focused, scale] = f[..] else { return None };
                let scale = scale.parse::<f64>().ok()?.max(0.1);
                Some((name.to_owned(), [x.parse().ok()?, y.parse().ok()?, (w.parse::<f64>().ok()? / scale) as i32, (h.parse::<f64>().ok()? / scale) as i32]))
            })
            .collect()
    };
    let _ = std::thread::Builder::new().name("session·cursor".into()).spawn(move || loop {
        if !CURSOR_WANTED.load(Ordering::Relaxed) {
            std::thread::sleep(std::time::Duration::from_millis(500));
            continue;
        }
        let Ok(stream) = std::os::unix::net::UnixStream::connect(format!("{dir}/cursor.sock")) else {
            std::thread::sleep(std::time::Duration::from_secs(2));
            continue;
        };
        let mut monitors = monitors_of(&dir);
        let mut asked = std::time::Instant::now();
        for line in std::io::BufReader::new(stream).lines() {
            let Ok(line) = line else { break };
            if !CURSOR_WANTED.load(Ordering::Relaxed) {
                break;
            }
            let Some((x, y)) = line.split_once(' ').and_then(|(x, y)| Some((x.parse::<f32>().ok()?, y.parse::<f32>().ok()?))) else { continue };
            if asked.elapsed().as_secs() >= 2 {
                asked = std::time::Instant::now();
                monitors = monitors_of(&dir);
            }
            if to_render.send(crate::scene::ToRender::Cursor((x, y), monitors.clone())).is_err() {
                return;
            }
        }
        // The session restarted it, or it went: try again in a moment.
        std::thread::sleep(std::time::Duration::from_secs(1));
    });
}

/// Where the scene is shown and the input comes from. pleamar's own is a
/// client of the system's compositor (Wayland); whoever builds on pleamar can
/// hand over another —pleamar-wm drives the monitors itself— with
/// `provide_platform`, before `run`.
pub trait Platform: Send {
    /// Puts up the surfaces the scene asks for, hands them to the render as
    /// sheets (`ToRender::Sheet`), and tells it the input, until the end.
    fn run(self: Box<Self>, surfaces: Vec<Surface>, extra_height: u32, instance: wgpu::Instance, to_render: std::sync::mpsc::Sender<ToRender>);
}

static PLATFORM: std::sync::Mutex<Option<Box<dyn Platform>>> = std::sync::Mutex::new(None);

pub fn provide_platform(p: Box<dyn Platform>) {
    *PLATFORM.lock().unwrap() = Some(p);
}

/// The platform handed over, if there is one.
pub fn provided_platform() -> Option<Box<dyn Platform>> {
    PLATFORM.lock().unwrap().take()
}

/// The keyboard layout pleamar's own window was given, as xkb text.
static HOST_KEYMAP: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

pub fn set_host_keymap(k: String) {
    *HOST_KEYMAP.lock().unwrap() = Some(k);
}

pub fn host_keymap() -> Option<String> {
    HOST_KEYMAP.lock().unwrap().clone()
}

/// The compositor inside the scene (`windows`): where to tell it things.
pub type NestSender = Box<dyn Fn(crate::scene::ToNest) + Send>;

/// What starts one: given how many windows the scene holds and where to
/// report to the render, it answers where to talk to it —or nothing, if it
/// could not start—.
pub type NestStarter = fn(usize, std::sync::mpsc::Sender<crate::scene::ToRender>) -> Option<NestSender>;

static NEST: std::sync::OnceLock<NestStarter> = std::sync::OnceLock::new();

/// pleamar itself holds no compositor: whoever builds one on top of it
/// (pleamar-wm) hands it over here before `run()`, and the scenes' `windows`
/// fill from it.
pub fn provide_windows(start: NestStarter) {
    let _ = NEST.set(start);
}

pub fn start_nest(max: usize, to_render: std::sync::mpsc::Sender<crate::scene::ToRender>) -> Option<NestSender> {
    match NEST.get() {
        Some(start) => start(max, to_render),
        None => {
            eprintln!("windows · this pleamar holds no other programs' windows: open the scene with pleamar-wm");
            None
        }
    }
}

/// The sounds a scene plays itself: its mixer, and how files and tones are made.
pub mod sound;
#[cfg(target_os = "linux")]
mod gamepad;
/// How game controller number `n` (from 1) is right now: `{ name, lx, ly, a, b, … }`.
#[cfg(target_os = "linux")]
pub use gamepad::state as gamepad;
#[cfg(not(target_os = "linux"))]
pub fn gamepad(_: usize) -> Option<SysValue> {
    None
}
#[cfg(target_os = "linux")]
mod auth;
#[cfg(target_os = "linux")]
mod notifications;
#[cfg(target_os = "linux")]
mod polkit;
#[cfg(target_os = "linux")]
mod tray;
#[cfg(target_os = "linux")]
mod compositor;
#[cfg(target_os = "linux")]
mod thumbnails;
/// A live window's last frame (`thumbnails.live`), by the `picture` that
/// names it, for the image that draws it.
#[cfg(target_os = "linux")]
pub use thumbnails::{frame as thumbnail_frame, Frame as ThumbnailFrame};
#[cfg(not(target_os = "linux"))]
pub struct ThumbnailFrame {
    pub version: u64,
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
    pub opaque: bool,
}
#[cfg(not(target_os = "linux"))]
pub fn thumbnail_frame(_: &str) -> Option<std::sync::Arc<ThumbnailFrame>> {
    None
}
mod files;

/// JSON to what the logic sees, and back: `json.decode` and `json.encode`.
pub fn json_decode(text: &str) -> Result<SysValue, String> {
    serde_json::from_str(text).map(files::from_json).map_err(|e| e.to_string())
}

pub fn json_encode(v: &SysValue) -> String {
    files::to_json(v).to_string()
}
mod clock;
#[cfg(target_os = "linux")]
mod desktop;
#[cfg(target_os = "linux")]
mod hyprland;
#[cfg(target_os = "linux")]
mod mpris;
#[cfg(target_os = "linux")]
mod networkmanager;
#[cfg(target_os = "linux")]
mod iwd;
#[cfg(target_os = "linux")]
mod bluez;
#[cfg(target_os = "linux")]
mod pulse;
#[cfg(target_os = "linux")]
mod system;
#[cfg(target_os = "linux")]
mod wayland;
#[cfg(target_os = "linux")]
pub use wayland::run_event_loop;

/// No platform yet: the core compiles —it's the guard that it stays
/// portable— but there's nowhere to paint.
#[cfg(not(target_os = "linux"))]
pub fn run_event_loop(_: Vec<Surface>, _: u32, _: wgpu::Instance, _: Sender<ToRender>) {
    eprintln!("pleamar cannot put windows on this system yet: its src/platform/ is missing");
    std::process::exit(1);
}

/// Where an icon's file is, by its name.
#[cfg(target_os = "linux")]
pub use wayland::icon;
#[cfg(not(target_os = "linux"))]
pub fn icon(_: &str) -> Option<std::path::PathBuf> {
    None
}
