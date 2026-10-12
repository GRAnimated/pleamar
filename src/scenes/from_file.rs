//! A scene that comes from a file in the language, and that reloads by itself
//! when the file changes. The logic that comes with it is the minimum: it listens.

use crate::scene::*;
use crate::logic::{Context, Script};
use std::sync::mpsc::Sender;
use std::time::Duration;

/// Reads and compiles a scene. The error already comes with its file, its line and its arrow.
/// A scene compiled a moment ago to look at it (`script_for`) is not compiled again.
pub fn read(path: &str) -> Result<Scene, String> {
    let ready = READY.lock().unwrap().take();
    match ready {
        Some((p, e)) if p == path => Ok(e),
        _ => compile(path),
    }
}

/// A big scene costs hundreds of MB to compile (pleamar-wm's session): at
/// start it was compiled three times —to see whether it had logic, to show
/// it, and to know which files to watch—, two of them at once.
static READY: std::sync::Mutex<Option<(String, Scene)>> = std::sync::Mutex::new(None);
/// The files the last compilation of each scene read: what to watch.
static FILES: std::sync::Mutex<Option<(String, Vec<std::path::PathBuf>)>> = std::sync::Mutex::new(None);

fn compile(path: &str) -> Result<Scene, String> {
    let (e, mut files) = crate::language::read_file(path)?;
    files.extend(e.attachments.iter().cloned());
    *FILES.lock().unwrap() = Some((path.to_owned(), files));
    Ok(e)
}

pub struct FromFile {
    path: String,
}

impl FromFile {
    pub fn new(path: &str) -> Self {
        FromFile { path: path.to_owned() }
    }
}

impl Script for FromFile {
    fn scene(&mut self) -> Scene {
        read(&self.path).unwrap_or_else(|m| {
            eprintln!("{m}");
            std::process::exit(1)
        })
    }

    fn on_event(&mut self, e: Event, c: &mut Context) {
        match e {
            // Without a mouse, `--demo` fires the "demo" signal: the scene will say what it does with it.
            Event::Demo => c.signal("demo"),
            Event::Layer(_, _) => c.work(),
            Event::Text(n, v) => println!("logic  · '{n}' now says: {v:?}"),
            Event::Submit(n, v) => println!("logic  · enter in '{n}': {v:?}"),
            Event::Received(z, kind, d) => println!("logic  · something was dropped on '{z}', a {kind}: {d:?}"),
            Event::Signal(s, None) => println!("logic  · '{s}' happened"),
            Event::Signal(s, Some(v)) => println!("logic  · '{s}' happened, with {v:.2}"),
            _ => {}
        }
    }
}

/// The script for a file scene: if next to it there is a `.luau` with the same
/// name, that is its logic; if not, one that only listens.
pub fn script_for(path: &str, tx: Sender<ToRender>, to_logic: Sender<Event>, blocked: std::sync::Arc<std::sync::atomic::AtomicBool>) -> Box<dyn Script> {
    let logic = logic_path_for(path);
    // There is logic if the scene has its `.luau`, if any of its plugins has its own,
    // or if the scene asks for some service: the script also sets that up and hands it out,
    // even if there is not a single line of Luau.
    #[cfg(feature = "luau")]
    {
        if std::path::Path::new(&logic).is_file() {
            return Box::new(crate::logic_luau::LuauScript::new(path, &logic, tx, to_logic, blocked));
        }
        // Compiled to look at it, and kept for the script to show it.
        if let Ok(e) = compile(path) {
            let wants_logic = !e.plugins.is_empty() || !e.services.is_empty();
            *READY.lock().unwrap() = Some((path.to_owned(), e));
            if wants_logic {
                return Box::new(crate::logic_luau::LuauScript::new(path, &logic, tx, to_logic, blocked));
            }
        }
    }
    let _ = (tx, to_logic, blocked, logic);
    Box::new(FromFile::new(path))
}

fn logic_path_for(path: &str) -> String {
    std::path::Path::new(path).with_extension("luau").to_string_lossy().into_owned()
}

/// Hot reload: it checks the file's date four times per second —which
/// works the same on any system— and, if it changed, reads it again. If it is
/// fine, the new scene replaces the old one without losing what was moving; if not,
/// it says why and the old one stays.
/// The program itself: if the binary changes —you just recompiled—, it relaunches
/// with the same arguments.
///
/// A live process keeps the compiler it was born with, so a
/// scene that uses something new in the language will not compile for it however many times it is
/// saved: it keeps the last good one and says so, which is correct, but
/// it looks as if hot reload does not work. This closes that hole.
///
/// `PLEAMAR_NO_RELAUNCH=1` turns it off, for whoever does not want updating the
/// package to restart their bar.
fn watch_binary() {
    if std::env::var_os("PLEAMAR_NO_RELAUNCH").is_some() || super::STAY_ON_UPDATE.load(std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    let Ok(me) = std::env::current_exe() else { return };
    let date = |r: &std::path::Path| std::fs::metadata(r).and_then(|m| m.modified()).ok();
    let Some(mut last) = date(&me) else { return };
    let _ = std::thread::Builder::new().name("relaunch".into()).spawn(move || loop {
        std::thread::sleep(Duration::from_millis(400));
        let Some(now) = date(&me) else { continue };
        if now == last {
            continue;
        }
        last = now;
        // Writing a binary is not instantaneous: it waits for it to stay still,
        // or it would relaunch with half a file.
        loop {
            std::thread::sleep(Duration::from_millis(150));
            match date(&me) {
                Some(d) if d == last => break,
                Some(d) => last = d,
                None => continue,
            }
        }
        println!("pleamar · the program changed on disk: starting again");
        let args: Vec<String> = std::env::args().skip(1).collect();
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            let error = std::process::Command::new(&me).args(&args).exec();
            eprintln!("pleamar · could not start again: {error}");
        }
        #[cfg(not(unix))]
        {
            let _ = std::process::Command::new(&me).args(&args).spawn();
            std::process::exit(0);
        }
    });
}

pub fn watch(path: String, to_render: Sender<ToRender>, to_logic: Sender<Event>) {
    watch_binary();
    // The logic reloads too: same trick, another file.
    let logic = logic_path_for(&path);
    let to_the_logic = to_logic.clone();
    let scene_for_logic = path.clone();
    std::thread::Builder::new()
        .name("reload-logic".into())
        .spawn(move || {
            // The scene's and its plugins': touching any of them reloads them.
            let all = |scene: &str| -> Vec<String> {
                let mut v = vec![logic.clone()];
                if let Ok(e) = read(scene) {
                    v.extend(e.plugins.iter().map(|p| p.logic.to_string_lossy().into_owned()));
                }
                v
            };
            let date = |v: &[String]| v.iter().map(|r| std::fs::metadata(r).and_then(|m| m.modified()).ok()).collect::<Vec<_>>();
            // And the modules it loads with `require`, as it loads them.
            let with_modules = |v: &[String]| -> Vec<String> {
                #[allow(unused_mut)]
                let mut v = v.to_vec();
                #[cfg(feature = "luau")]
                v.extend(crate::logic_luau::REQUIRED.lock().unwrap().iter().map(|p| p.to_string_lossy().into_owned()));
                v
            };
            let mut watched = all(&scene_for_logic);
            let mut last = date(&with_modules(&watched));
            loop {
                std::thread::sleep(Duration::from_millis(250));
                let now = date(&with_modules(&watched));
                // A module required for the first time: watched from here on.
                if now.len() != last.len() {
                    last = now;
                    continue;
                }
                if now != last && now.iter().any(Option::is_some) {
                    std::thread::sleep(Duration::from_millis(80));
                    watched = all(&scene_for_logic);
                    last = date(&with_modules(&watched));
                    if to_logic.send(Event::ReloadLogic).is_err() {
                        return;
                    }
                }
            }
        })
        .unwrap();
    std::thread::Builder::new()
        .name("reload".into())
        .spawn(move || {
            // The scene and whatever it imports: touching a library also reloads it.
            // The scene, its libraries and whatever they bring attached —the SVGs of their
            // figures—: you draw the hat, save, and it is on.
            // Those the scene's compilation read, if it has already been: not compiled again for this.
            let known = FILES.lock().unwrap().take().filter(|(p, _)| *p == path).map(|(_, f)| f);
            let mut watched: Vec<std::path::PathBuf> = known.unwrap_or_else(|| {
                crate::language::read_file(&path).map_or_else(
                    |_| vec![path.clone().into()],
                    |(e, mut f)| {
                        f.extend(e.attachments.iter().cloned());
                        f
                    },
                )
            });
            let date = |v: &[std::path::PathBuf]| v.iter().map(|r| std::fs::metadata(r).and_then(|m| m.modified()).ok()).collect::<Vec<_>>();
            let mut last = date(&watched);
            loop {
                std::thread::sleep(Duration::from_millis(250));
                let mut now = date(&watched);
                if now == last || now.iter().all(Option::is_none) {
                    continue;
                }
                // An editor saves in several steps —truncates, writes, sometimes renames—:
                // it waits for the file to have been still for a moment and not be empty.
                loop {
                    std::thread::sleep(Duration::from_millis(60));
                    let after = date(&watched);
                    // Only what is on disk: a library of pleamar's own
                    // (`pleamar:ui`) has no file, and taking it for one half
                    // saved left this waiting forever —no reload ever again—.
                    let empty = watched.iter().any(|r| std::fs::metadata(r).is_ok_and(|m| m.len() == 0));
                    if after == now && !empty {
                        break;
                    }
                    now = after;
                }
                let t0 = std::time::Instant::now();
                match crate::language::read_file(&path) {
                    Ok((e, files)) => {
                        println!("reload · {path} read in {:.1} ms{}", t0.elapsed().as_secs_f32() * 1000.0, if files.len() > 1 { format!(" · with {} more files", files.len() - 1) } else { String::new() });
                        // It may import other things now.
                        watched = files;
                        watched.extend(e.attachments.iter().cloned());
                        let _ = to_render.send(ToRender::ReloadError(None));
                        let _ = to_the_logic.send(Event::NewScene(e.facts.clone(), e.texts.clone(), e.permissions.clone(), e.models.clone(), e.types.clone(), e.plugins.clone(), e.signals.iter().map(|s| s.0).collect(), e.services.clone(), e.translations.clone()));
                        if to_render.send(ToRender::Scene(e)).is_err() {
                            return;
                        }
                    }
                    Err(m) => {
                        eprintln!("reload · the scene stays as it was:\n{m}");
                        let _ = to_render.send(ToRender::ReloadError(Some(m)));
                    }
                }
                last = date(&watched);
            }
        })
        .unwrap();
}

#[cfg(test)]
mod tests {
    use crate::scene::{Rule, Trigger};

    /// A zone made in a `repeat` of a scene with one copy per monitor: each copy
    /// has its own, and each copy's rule presses its own. Both used to be one
    /// name, the second copy's, and the first monitor's zone was deaf.
    #[test]
    fn repeated_zones_belong_to_their_screen_copy() {
        let dir = std::env::temp_dir().join(format!("pleamar-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("copies.plm");
        std::fs::write(&path, "scene Copies {
    surface { size: 200, 40; anchor: top; screens: each max 2 }
    fact hit = 0
    repeat k in 1..3 {
        prop glow.$k = 0 ~140ms
        zone box glow.$k { at: 40 * k, 20; size: 30, 30 }
        on press glow.$k { hit = k; glow.$k: 1 }
    }
}
").unwrap();
        let scene = super::read(path.to_str().unwrap()).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        let ids: Vec<&str> = scene.zones.iter().map(|z| z.id).collect();
        for name in ["glow.1#screen0", "glow.1#screen1", "glow.2#screen0", "glow.2#screen1"] {
            assert!(ids.contains(&name), "no zone {name} in {ids:?}");
        }
        let pressed: Vec<&str> = scene.rules.iter().filter_map(|r: &Rule| match r.when {
            Trigger::Press(z) => Some(scene.zones[z.0 as usize].id),
            _ => None,
        }).collect();
        for name in ["glow.1#screen0", "glow.1#screen1"] {
            assert!(pressed.contains(&name), "no rule presses {name}: {pressed:?}");
        }
    }

    /// A `for` inside a `grid`: a cell past the end of its list is neither
    /// seen nor pressed, as in a row or a column. It used to keep showing the
    /// records of the last longer list.
    #[test]
    fn a_grid_only_has_the_cells_its_list_reaches() {
        let dir = std::env::temp_dir().join(format!("pleamar-grid-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("grid.plm");
        std::fs::write(&path, "scene Cells {
    surface { size: 200, 100 }
    model rows max 4 { title: text }
    grid {
        at: 0, 0; columns: 2; width: 200; gap: 10; row: 40
        for r in rows {
            group {
                size: 95, 40
                zone box hit { from: 0, 0; size: 95, 40 }
                text r.title { at: 4, 20; size: 12; color: #fff }
            }
        }
    }
}
").unwrap();
        let scene = super::read(path.to_str().unwrap()).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        let mut facts: Vec<f32> = scene.facts.iter().map(|f| f.1).collect();
        let count = scene.facts.iter().position(|f| f.0 == "rows.count").expect("rows.count");
        let active = |facts: &[f32]| scene.zones.iter().filter(|z| z.active.is_true(crate::scene::Ctx { props: &[], facts, seen: None })).count();
        assert_eq!(scene.zones.len(), 4);
        facts[count] = 4.0;
        assert_eq!(active(&facts), 4);
        facts[count] = 1.0;
        assert_eq!(active(&facts), 1, "a cell the list does not reach can still be pressed");
        facts[count] = 0.0;
        assert_eq!(active(&facts), 0);
    }

    /// A rule that names nothing of its copy is the same rule once per monitor:
    /// only the first one may act, or a count goes up twice per monitor.
    #[test]
    fn a_shared_rule_acts_once_whatever_the_copies() {
        let dir = std::env::temp_dir().join(format!("pleamar-twins-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("twins.plm");
        std::fs::write(&path, "scene Twins {
    surface { size: 200, 40; anchor: top; screens: each max 2 }
    event landed
    fact n = 0
    on landed { n = n + 1 }
    zone box hit { from: 0, 0; size: 20, 20 }
    on press hit { n = 0 }
}
").unwrap();
        let scene = super::read(path.to_str().unwrap()).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        let twins: Vec<usize> = (0..scene.rules.len()).filter(|&k| scene.twin_of[k] != k).collect();
        // The count is one rule written twice; the press is each copy's own zone.
        assert_eq!(twins.len(), 1, "twins: {:?}", scene.twin_of);
        assert!(matches!(scene.rules[twins[0]].when, crate::scene::Trigger::On(_)));
    }

    /// A big `let` is computed in a property: read in every monitor's copy,
    /// it is one property, or the rules naming it differ per copy and each
    /// copy's acts —a `toggle` undoes itself—.
    #[test]
    fn a_big_let_is_one_property_whatever_the_copies() {
        let dir = std::env::temp_dir().join(format!("pleamar-twin-lets-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("lets.plm");
        std::fs::write(&path, "scene Lets {
    surface { size: 200, 40; anchor: top; screens: each max 2 }
    event go
    fact a = 0
    fact b = 0
    fact t = 0
    repeat i in 0..1 {
        let big = max(a, b) + (a - b) * 3 + min(a * 2, b) + abs(a)
        on go { t = if(big > 1, 1 - t, t) }
    }
}
").unwrap();
        let scene = super::read(path.to_str().unwrap()).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        let bound = scene.behaviors.iter().filter(|b| matches!(b, crate::scene::Behavior::Bind { .. })).count();
        assert_eq!(bound, 1, "one property for the let, not one per copy");
        let twins: Vec<usize> = (0..scene.rules.len()).filter(|&k| scene.twin_of[k] != k).collect();
        assert_eq!(twins.len(), 1, "twins: {:?}", scene.twin_of);
    }
}
