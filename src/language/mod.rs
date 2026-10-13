//! The scene language. A text file goes in and a `Scene` comes out —the same
//! one that until now was written in Rust—, or an error that says where and
//! why. Everything declared here is run by the render, on its own.
//!
//! Three steps: `tokens` chops, `tree` groups into nodes and properties without
//! knowing what they mean, and `compiler` gives them meaning and checks the names.

mod compiler;
mod figure;
mod tokens;
mod tree;
pub mod vocabulary;

use crate::scene::Scene;
use std::path::{Path, PathBuf};

/// A scene can be made of several files (`import`), and an error has to say
/// which one. So as not to load every token with a name, the line number
/// carries the file's inside it: line 12 of the third one is 2 000 012.
pub(crate) const PER_FILE: usize = 1_000_000;

/// The version of the language this program understands. The first number changes
/// when something written stops being valid; the second, when something is added. A file
/// can say which one it needs (`language 0.1`) and find out on load, not halfway through.
pub const VERSION: (u32, u32) = (0, 4);

#[derive(Debug)]
pub struct CompileError {
    pub line: usize,
    pub col: usize,
    pub message: String,
}

impl CompileError {
    pub fn at(line: usize, col: usize, message: impl Into<String>) -> CompileError {
        CompileError { line, col, message: message.into() }
    }

    /// With the file's line and an arrow underneath, the way a compiler shows it.
    fn with_source(&self, files: &[(PathBuf, String)]) -> String {
        let (path, source) = files.get(self.line / PER_FILE).or(files.first()).map_or((String::new(), ""), |(r, f)| {
            // Relative to where we are, if possible: a whole path gets in the way more than it helps.
            let short = std::env::current_dir().ok().and_then(|here| r.strip_prefix(here).ok().map(Path::to_owned)).unwrap_or_else(|| r.clone());
            (short.display().to_string(), f.as_str())
        });
        let n = self.line % PER_FILE;
        let line = source.lines().nth(n.saturating_sub(1)).unwrap_or("");
        let margin = format!("{n:>4} | ");
        format!("{path}:{n}:{}: {}\n{margin}{line}\n{}^", self.col, self.message, " ".repeat(margin.len() + self.col.saturating_sub(1)))
    }
}

/// "file:line" of something that was declared, to say where the thing being overridden was.
pub fn location(names: &[String], line: usize) -> String {
    format!("{}:{}", names.get(line / PER_FILE).map_or("", String::as_str), line % PER_FILE)
}

/// What has been read so far: each file once, with its text to show the errors.
#[derive(Default)]
struct Reader {
    files: Vec<(PathBuf, String)>,
    /// The ones being read right now, one inside another: to spot cycles.
    open_stack: Vec<PathBuf>,
    /// Which files are `strict` libraries, by their number.
    strict: Vec<usize>,
    /// Each library: its file number, its name, and its logic if it has a `.luau` next to it.
    libraries: Vec<compiler::Library>,
    /// What an editor has open and has not saved yet: it wins over the disk.
    unsaved: Vec<(PathBuf, String)>,
    /// The name each library was imported with, `as menu`, if any.
    surnames: Vec<(PathBuf, Option<String>)>,
}

impl Reader {
    /// Tokenizes and groups a file, with its number put into the lines.
    fn open(&mut self, path: &Path) -> Result<Vec<tree::Entry>, CompileError> {
        let k = self.files.len();
        let canonical = path.canonicalize().unwrap_or_else(|_| path.to_owned());
        let source = match self.unsaved.iter().find(|(r, _)| *r == canonical) {
            Some((_, t)) => t.clone(),
            None => std::fs::read_to_string(path).map_err(|e| CompileError::at(0, 0, format!("cannot read {}: {e}", path.display())))?,
        };
        self.files.push((path.to_owned(), source));
        let here = |mut f: CompileError| { f.line += k * PER_FILE; f };
        let mut tokens = tokens::tokenize(&self.files[k].1).map_err(here)?;
        for f in &mut tokens {
            f.line += k * PER_FILE;
        }
        // Every file says which version it needs, a library too.
        check_version(tree::parse(&tokens).map_err(|f| if f.line < PER_FILE { here(f) } else { f })?)
    }

    /// `include "pages/wifi.plm"`: a piece of the scene kept in a file of its
    /// own (`part Wifi { … }`), put where the line is —inside a group, if that
    /// is where it is written—, as if it had been written there: the same
    /// names, the same order, its lines still its own file's, for the errors and
    /// for the paths of what it draws. A part can include others; it does not
    /// import: libraries are the scene's business.
    fn resolve_includes(&mut self, path: &Path, entries: &mut Vec<tree::Entry>) -> Result<(), CompileError> {
        use tokens::TokenKind;
        let mut out = Vec::with_capacity(entries.len());
        for mut e in std::mem::take(entries) {
            let tree::Entry::Node(n) = &mut e else {
                out.push(e);
                continue;
            };
            if !matches!(n.head.first().map(|f| &f.kind), Some(TokenKind::Id(p)) if p == "include") {
                if let Some(body) = n.body.as_mut() {
                    self.resolve_includes(path, body)?;
                }
                out.push(e);
                continue;
            }
            let (Some(TokenKind::Str(which)), 2, None) = (n.head.get(1).map(|f| &f.kind), n.head.len(), &n.body) else {
                return Err(CompileError::at(n.line, n.col, "an include is `include \"path/to/the/part.plm\"`: a piece of this scene, in a file of its own"));
            };
            let target = path.parent().unwrap_or(Path::new(".")).join(which);
            let target = target.canonicalize().map_err(|e| CompileError::at(n.line, n.head[1].col, format!("cannot find '{which}' (looking in {}): {e}", target.display())))?;
            if self.open_stack.contains(&target) {
                return Err(CompileError::at(n.line, n.head[1].col, format!("'{which}' ends up including itself: {}", self.open_stack.iter().chain([&target]).map(|p| p.file_name().unwrap_or_default().to_string_lossy()).collect::<Vec<_>>().join(" → "))));
            }
            self.open_stack.push(target.clone());
            let theirs = self.open(&target)?;
            let part = match theirs.as_slice() {
                [tree::Entry::Node(p)] if matches!(p.head.first().map(|f| &f.kind), Some(TokenKind::Id(w)) if w == "part") => p,
                [tree::Entry::Node(p)] if matches!(p.head.first().map(|f| &f.kind), Some(TokenKind::Id(w)) if w == "library") => {
                    return Err(CompileError::at(n.line, n.head[1].col, format!("'{which}' is a library: a library is imported (`import \"{which}\"`), and a part, included")));
                }
                _ => return Err(CompileError::at(n.line, n.head[1].col, format!("'{which}' is not a part: what is included is `part Name {{ … }}`, a piece of a scene, and nothing else"))),
            };
            if !matches!((part.head.get(1).map(|f| &f.kind), part.head.len()), (Some(TokenKind::Id(_)), 2)) {
                return Err(CompileError::at(part.line, part.col, "a part has a name, and only that: `part Wifi { … }`"));
            }
            let mut body = part.body.clone().unwrap_or_default();
            if let Some(tree::Entry::Node(i)) = body.iter().find(|d| matches!(d, tree::Entry::Node(x) if matches!(x.head.first().map(|f| &f.kind), Some(TokenKind::Id(p)) if p == "import"))) {
                return Err(CompileError::at(i.line, i.col, "a part does not import: libraries are imported by the scene, and then the part can use them"));
            }
            self.resolve_includes(&target, &mut body)?;
            self.open_stack.pop();
            out.extend(body);
        }
        *entries = out;
        Ok(())
    }

    /// What a file's `import`s declare, in order, and what is left of it.
    /// A library imported twice —by two routes— is read once.
    fn resolve_imports(&mut self, path: &Path, entries: Vec<tree::Entry>, brought: &mut Vec<tree::Entry>) -> Result<Vec<tree::Entry>, CompileError> {
        use tokens::TokenKind;
        let mut rest = Vec::new();
        for e in entries {
            let tree::Entry::Node(n) = &e else { rest.push(e); continue };
            if !matches!(n.head.first().map(|f| &f.kind), Some(TokenKind::Id(p)) if p == "import") {
                rest.push(e);
                continue;
            }
            // `import "menu.plm" as menu`: its components are `menu.Row`, so two
            // libraries with a `Row` each can live in the same scene.
            let surname = match (n.head.get(2).map(|f| &f.kind), n.head.get(3).map(|f| &f.kind), n.head.len()) {
                (None, _, 2) => None,
                (Some(TokenKind::Id(a)), Some(TokenKind::Id(s)), 4) if a == "as" && !s.contains('.') => Some(s.clone()),
                _ => None,
            };
            let (Some(TokenKind::Str(which)), None, true) = (n.head.get(1).map(|f| &f.kind), &n.body, n.head.len() == 2 || surname.is_some()) else {
                return Err(CompileError::at(n.line, n.col, "an import is `import \"path/to/the/library.plm\"`, or `import \"…\" as name` to call its components `name.Row`"));
            };
            // `pleamar:ui` is one of pleamar's own libraries: it comes inside the
            // program, so it is there wherever the scene is and whoever runs it.
            // Anything else is a path, relative to the importing file, not to
            // where it is launched from.
            let target = if let Some(name) = which.strip_prefix("pleamar:") {
                let Some(source) = builtin_library(name) else {
                    return Err(CompileError::at(n.line, n.head[1].col, format!("pleamar has no library '{name}': there is {}", BUILTIN_LIBRARIES.iter().map(|(n, _)| format!("pleamar:{n}")).collect::<Vec<_>>().join(", "))));
                };
                let t = PathBuf::from(format!("pleamar:{name}.plm"));
                if !self.unsaved.iter().any(|(r, _)| *r == t) {
                    self.unsaved.push((t.clone(), source.to_owned()));
                }
                t
            } else {
                let target = path.parent().unwrap_or(Path::new(".")).join(which);
                target.canonicalize().map_err(|e| CompileError::at(n.line, n.head[1].col, format!("cannot find '{which}' (looking in {}): {e}", target.display())))?
            };
            if self.open_stack.contains(&target) {
                return Err(CompileError::at(n.line, n.head[1].col, format!("'{which}' ends up importing itself: {}", self.open_stack.iter().chain([&target]).map(|p| p.file_name().unwrap_or_default().to_string_lossy()).collect::<Vec<_>>().join(" → "))));
            }
            if let Some((_, before)) = self.surnames.iter().find(|(r, _)| *r == target) {
                if *before != surname {
                    let said = |s: &Option<String>| s.as_ref().map_or("with no name".to_owned(), |s| format!("as {s}"));
                    return Err(CompileError::at(n.line, n.col, format!("'{which}' is already imported {}, and here {}: a library has one name in a scene", said(before), said(&surname))));
                }
            }
            if self.files.iter().any(|(r, _)| r == &target) {
                continue;
            }
            self.surnames.push((target.clone(), surname.clone()));
            self.open_stack.push(target.clone());
            let number = self.files.len();
            let theirs = self.open(&target)?;
            let theirs = self.resolve_imports(&target, theirs, brought)?;
            self.open_stack.pop();
            let [tree::Entry::Node(b)] = theirs.as_slice() else {
                return Err(CompileError::at(n.line, n.head[1].col, format!("'{which}' is not a library: it has to be `library Name {{ … }}`, and nothing else")));
            };
            if !matches!(b.head.first().map(|f| &f.kind), Some(TokenKind::Id(p)) if p == "library") {
                return Err(CompileError::at(b.line, b.col, "what you import is a library: `library Name { … }`. A scene is not imported"));
            }
            let Some(tree::Entry::Node(b)) = theirs.into_iter().next() else { unreachable!() };
            let TokenKind::Id(library_name) = &b.head.get(1).map(|f| f.kind.clone()).unwrap_or(TokenKind::Id(String::new())) else {
                return Err(CompileError::at(b.line, b.col, "this library is missing its name: `library Name { … }`"));
            };
            let logic = target.with_extension("luau");
            self.libraries.push(compiler::Library { file: number, name: library_name.clone(), logic: logic.is_file().then_some(logic) });
            // `library Menu strict { … }`: its components only read what they ask for.
            match b.head.get(2).map(|f| &f.kind) {
                None => {}
                Some(TokenKind::Id(p)) if p == "strict" && b.head.len() == 3 => self.strict.push(number),
                Some(_) => return Err(CompileError::at(b.line, b.head[2].col, "after a library name only `strict` can go")),
            }
            let mut body = b.body.unwrap_or_default();
            if let Some(surname) = &surname {
                let own: Vec<String> = body
                    .iter()
                    .filter_map(|d| match d {
                        tree::Entry::Node(x) if matches!(x.head.first().map(|f| &f.kind), Some(TokenKind::Id(p)) if p == "component") => match x.head.get(1).map(|f| &f.kind) {
                            Some(TokenKind::Id(c)) => Some(c.clone()),
                            _ => None,
                        },
                        _ => None,
                    })
                    .collect();
                // Where it is declared and wherever the library itself uses it.
                fn rename(entries: &mut [tree::Entry], own: &[String], surname: &str) {
                    let fix = |tokens: &mut [tokens::Token]| {
                        for t in tokens {
                            if let TokenKind::Id(w) = &mut t.kind {
                                if own.contains(w) {
                                    *w = format!("{surname}.{w}");
                                }
                            }
                        }
                    };
                    for e in entries {
                        match e {
                            tree::Entry::Prop { value, .. } => fix(value),
                            tree::Entry::Node(x) => {
                                fix(&mut x.head);
                                rename(x.body.as_deref_mut().unwrap_or(&mut []), own, surname);
                            }
                        }
                    }
                }
                rename(&mut body, &own, surname);
            }
            for d in body {
                // A library declares; it does not paint, or react, or have a frontier with the logic.
                // `text now = "…"` declares; `text "hello" { … }` paints, and that is not a library's business.
                let ok = matches!(&d, tree::Entry::Node(x) if matches!(x.head.first().map(|f| &f.kind), Some(TokenKind::Id(p)) if vocabulary::LIBRARY_STATEMENTS.contains(&p.as_str())
                    && (!["text", "image"].contains(&p.as_str()) || matches!(x.head.get(2).map(|f| &f.kind), Some(TokenKind::Sym("="))))));
                if !ok {
                    let (l, c) = match &d { tree::Entry::Node(x) => (x.line, x.col), tree::Entry::Prop { line, col, .. } => (*line, *col) };
                    return Err(CompileError::at(l, c, "a library only declares: `let`, `spring`, `component`, its frontier (`fact`, `text`, `model`, `event`, `image x = …`, `permissions`) and what it moves inside (`prop`, `pose`, `gesture`, `posture`, `layer`). What gets painted, and loose rules, belong to the scene"));
                }
                brought.push(d);
            }
        }
        Ok(rest)
    }
}

/// pleamar's own libraries, inside the program: `import "pleamar:ui"`.
const BUILTIN_LIBRARIES: &[(&str, &str)] = &[("ui", include_str!("../../lib/ui.plm"))];

fn builtin_library(name: &str) -> Option<&'static str> {
    BUILTIN_LIBRARIES.iter().find(|(n, _)| *n == name).map(|(_, s)| *s)
}

/// `language 0.1`, if present, is the first thing in the file. It is checked and removed.
fn check_version(mut entries: Vec<tree::Entry>) -> Result<Vec<tree::Entry>, CompileError> {
    use tokens::TokenKind;
    let Some(tree::Entry::Node(n)) = entries.first() else { return Ok(entries) };
    if !matches!(n.head.first().map(|f| &f.kind), Some(TokenKind::Id(p)) if p == "language") {
        return Ok(entries);
    }
    let (Some(TokenKind::Num(v)), 2, None) = (n.head.get(1).map(|f| &f.kind), n.head.len(), &n.body) else {
        return Err(CompileError::at(n.line, n.col, format!("the version is asked for like this: `language {}.{}`", VERSION.0, VERSION.1)));
    };
    // `0.1` arrives as a number: the integer part and the first decimal.
    let asked = (v.trunc() as u32, ((v.fract() * 10.0).round()) as u32);
    if asked.0 != VERSION.0 || asked.1 > VERSION.1 {
        return Err(CompileError::at(n.line, n.head[1].col, format!("this file asks for language {}.{}, and this pleamar understands {}.{}", asked.0, asked.1, VERSION.0, VERSION.1)));
    }
    entries.remove(0);
    Ok(entries)
}

/// Reads a scene from its file, with whatever it imports. Returns the scene and all the
/// files it is made of —to watch them—, or the errors already with their file,
/// their line and their arrow.
pub fn read_file(path: &str) -> Result<(Scene, Vec<PathBuf>), String> {
    read_with(path, Vec::new()).map_err(|(errors, files)| {
        let n = errors.len();
        let text: Vec<String> = errors.iter().map(|f| f.with_source(&files)).collect();
        format!("{}\n{}", text.join("\n\n"), if n == 1 { "one error".to_owned() } else { format!("{n} errors") })
    })
}

/// A diagnostic for whoever is writing, as is: in which file, where, and what is wrong.
/// It is what the language server sends the editor while typing.
pub struct Diagnostic {
    pub file: PathBuf,
    pub line: usize,
    pub col: usize,
    pub message: String,
}

pub use compiler::Symbol;

/// What is wrong and what there is: both in one pass, which is how the editor wants them.
/// The line of each name already comes without the file number, and with it apart.
pub fn index(path: &str, unsaved: Vec<(PathBuf, String)>) -> (Vec<Diagnostic>, Vec<(PathBuf, Symbol)>) {
    let (r, symbols) = read_with_symbols(path, unsaved);
    let files: Vec<PathBuf> = match &r {
        Ok((_, f)) => f.clone(),
        Err((_, f)) => f.iter().map(|(p, _)| p.clone()).collect(),
    };
    let file_of = |line: usize| files.get(line / PER_FILE).cloned().unwrap_or_else(|| PathBuf::from(path));
    let names = symbols
        .into_iter()
        .map(|mut s| {
            let f = file_of(s.line);
            s.line %= PER_FILE;
            (f, s)
        })
        .collect();
    (diagnostics_for(path, r), names)
}

fn diagnostics_for(path: &str, r: Result<(Scene, Vec<PathBuf>), (Vec<CompileError>, Vec<(PathBuf, String)>)>) -> Vec<Diagnostic> {
    match r {
        Ok(_) => Vec::new(),
        Err((errors, files)) => errors
            .into_iter()
            .map(|f| Diagnostic {
                file: files.get(f.line / PER_FILE).map_or_else(|| PathBuf::from(path), |(r, _): &(PathBuf, String)| r.clone()),
                line: f.line % PER_FILE,
                col: f.col,
                message: f.message,
            })
            .collect(),
    }
}

fn read_with(path: &str, unsaved: Vec<(PathBuf, String)>) -> Result<(Scene, Vec<PathBuf>), (Vec<CompileError>, Vec<(PathBuf, String)>)> {
    let (r, _) = read_with_symbols(path, unsaved);
    r
}

/// The same, and also the names the scene declares with their location: the editor
/// needs them even when the file is half-written and does not compile.
fn read_with_symbols(path: &str, unsaved: Vec<(PathBuf, String)>) -> (Result<(Scene, Vec<PathBuf>), (Vec<CompileError>, Vec<(PathBuf, String)>)>, Vec<compiler::Symbol>) {
    let mut l = Reader { unsaved, ..Default::default() };
    let main = Path::new(path).canonicalize().unwrap_or_else(|_| PathBuf::from(path));
    let mut symbols = Vec::new();
    let compiled = (|| {
        l.open_stack.push(main.clone());
        let mut entries = l.open(Path::new(path)).map_err(|f| vec![f])?;
        l.files[0].0 = main.clone();
        l.resolve_includes(&main, &mut entries).map_err(|f| vec![f])?;
        let mut brought = Vec::new();
        let mut rest = l.resolve_imports(&main, entries, &mut brought).map_err(|f| vec![f])?;
        // What is imported goes before the scene's own, as if it were written there.
        if let [tree::Entry::Node(scene)] = rest.as_mut_slice() {
            if let Some(body) = scene.body.as_mut() {
                brought.append(body);
                *body = brought;
            }
        }
        let names: Vec<String> = l.files.iter().map(|(r, _)| r.file_name().unwrap_or_default().to_string_lossy().into_owned()).collect();
        let dirs: Vec<PathBuf> = l.files.iter().map(|(r, _)| r.parent().map_or_else(PathBuf::new, Path::to_owned)).collect();
        let (r, s) = compiler::compile(&rest, &names, &dirs, &l.strict, &l.libraries);
        symbols = s;
        r
    })();
    // When showing an error, the main file with the path given by whoever opened it.
    if let Some(f) = l.files.first_mut() {
        f.0 = PathBuf::from(path);
    }
    let r = match compiled {
        Ok(e) => Ok((e, l.files.into_iter().map(|(r, _)| r).collect())),
        Err(errors) => Err((errors, l.files)),
    };
    (r, symbols)
}

/// "Did you mean…?": the known name that looks most alike, if it looks alike enough.
pub fn closest_match<'a>(a: &str, known: impl Iterator<Item = &'a String>) -> Option<&'a String> {
    let distance = |x: &str, y: &str| {
        let (x, y): (Vec<char>, Vec<char>) = (x.chars().collect(), y.chars().collect());
        let mut row: Vec<usize> = (0..=y.len()).collect();
        for i in 1..=x.len() {
            let mut prev = row[0];
            row[0] = i;
            for j in 1..=y.len() {
                let saved = row[j];
                row[j] = (prev + (x[i - 1] != y[j - 1]) as usize).min(row[j] + 1).min(row[j - 1] + 1);
                prev = saved;
            }
        }
        row[y.len()]
    };
    known.map(|c| (distance(a, c), c)).filter(|(d, c)| *d <= 1 + c.len() / 4).min_by_key(|(d, _)| *d).map(|(_, c)| c)
}

#[cfg(test)]
mod tests {
    use crate::scene::{Animated, Behavior, Ctx, Expr, Scene, Spring};
    use crate::shapes::Shape;

    /// The scene's bound properties, computed in their order with these facts and `t` at a half.
    fn settled(scene: &Scene, facts: &[f32]) -> Vec<Animated> {
        let mut props: Vec<Animated> = scene.props.iter().map(|(name, x, _)| Animated::at(if *name == "t" { 0.5 } else { *x }, Spring::LIVELY)).collect();
        for b in &scene.behaviors {
            if let Behavior::Bind { prop, to } = b {
                let v = to.eval(Ctx { props: &props, facts, seen: None });
                props[prop.0 as usize].x = v;
            }
        }
        props
    }

    #[test]
    fn a_chain_of_lets_from_the_screen_is_computed_once_per_copy() {
        let (scene, _) = super::read_file(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/chained-lets-per-screen.plm")).unwrap();
        // Each link names the previous one twice: substituted, the last one was
        // hundreds of nodes, and here nothing is more than its own line.
        let widths: Vec<(&str, &Expr)> = scene
            .zones
            .iter()
            .map(|z| match &z.shape {
                Shape::Rect { half_size, .. } => (z.id, &half_size.0),
                _ => panic!("the zone is a box"),
            })
            .collect();
        assert_eq!(widths.iter().map(|(id, _)| *id).collect::<Vec<_>>(), ["hit#screen0", "hit#screen1"], "one zone per monitor's copy");
        for (id, w) in &widths {
            assert!(w.node_count() <= 3, "{id} reads its width from a property, not from the whole chain: {w:?}");
        }
        for b in &scene.behaviors {
            if let Behavior::Bind { to, .. } = b {
                assert!(to.node_count() < 30, "a link reads the one before it from its property: {to:?}");
            }
        }
        // And each copy's is its own: a monitor half as tall has the chain half as big.
        let mut facts: Vec<f32> = scene.facts.iter().map(|(_, v)| *v).collect();
        let fact = |name: &str| scene.facts.iter().position(|(n, _)| *n == name).unwrap_or_else(|| panic!("no fact {name}"));
        facts[fact("screen.0.height")] = 1350.0;
        facts[fact("screen.1.height")] = 675.0;
        let props = settled(&scene, &facts);
        let expected = |height: f32| {
            let t = 0.5;
            let sc = (height / 1350.0).clamp(0.55, 1.0) * (1.0 + t * 0.2);
            let mut x = 10.0 * sc + (40.0 * sc - 10.0 * sc) * t;
            for k in [60.0, 80.0, 100.0, 120.0, 140.0] {
                x += (k * sc - x) * t;
            }
            x
        };
        for (id, w) in &widths {
            let height = if id.ends_with("#screen1") { 675.0 } else { 1350.0 };
            let got = w.eval(Ctx { props: &props, facts: &facts, seen: None }) * 2.0;
            assert!((got - expected(height)).abs() < 1e-2, "{id}: {got} and it had to be {}", expected(height));
        }
    }
}
