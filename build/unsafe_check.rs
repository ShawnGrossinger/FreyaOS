// unsafe_check.rs — every `unsafe` in the kernel says why it is sound, or
// the kernel doesn't build (Entry 30).
//
//   unsafe_check <kernel/src> <list>
//
// The Makefile compiles this for the build machine and runs it before the
// kernel is compiled. It writes the list (build/out/unsafe.txt; `make
// unsafe` prints it), or names each unsafe that breaks a rule below and
// fails, which stops the build.
//
// The rules, for every file in src/:
//
//   unsafe { ... }        A `// SAFETY: [category] why` comment in the
//   unsafe impl ...       comment lines directly above the line it is on.
//                         The category is one of CATEGORIES below.
//
//   unsafe fn ...         A `# Safety` section in its doc comment, saying
//   unsafe trait ...      what a caller must make true.
//
// main.rs has #![deny(unsafe_op_in_unsafe_fn)], so the body of an unsafe fn
// is not itself unsafe: every unsafe operation in the kernel is inside an
// unsafe block, and so carries its own SAFETY comment.
//
// The comments are the list the manifesto asks for (Phase A: "every unsafe
// block listed and justified"). The list is made from them whenever a
// source file changes, so it can't fall behind the code it describes.
//
// This reads Rust well enough to tell code from comments, strings and
// character literals. It is not a parser: it finds the word `unsafe` and
// looks at what follows it.

use std::fmt::Write as _;
use std::path::Path;

const CATEGORIES: &[(&str, &str)] = &[
    ("cpu",    "a privileged instruction: the interrupt flag, halting, control and model-specific registers, descriptor tables"),
    ("port",   "I/O port instructions (in, out) to a device"),
    ("mmio",   "a device's registers, or the firmware's tables, through memory the kernel mapped for them"),
    ("phys",   "memory by physical address: frames through the physical map, and what the boot stages left at fixed addresses"),
    ("paging", "page tables: building, changing or loading them, and memory reached through a mapping just made"),
    ("static", "kernel state in a `static mut`: one CPU, touched with interrupts off or by one task only"),
    ("heap",   "the kernel heap's allocator: its free list and the memory it hands out"),
    ("switch", "stacks and context switches, entering ring 3, and functions written in assembly"),
    ("user",   "a process's memory, read or written for a system call"),
    ("fault",  "a fault caused on purpose, by a self-test or a FREYA_DEMO build"),
];

#[derive(Clone, Copy, PartialEq)]
enum Kind { Block, Impl, Fn, Trait }

impl Kind {
    fn name(self) -> &'static str {
        match self { Kind::Block => "block", Kind::Impl => "impl", Kind::Fn => "fn", Kind::Trait => "trait" }
    }
}

struct Site {
    file: String,
    line: usize,
    kind: Kind,
    category: &'static str,
    why: String,
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 3 {
        eprintln!("usage: unsafe_check <kernel/src> <list>");
        std::process::exit(2);
    }
    let src = Path::new(&args[1]);
    let list = &args[2];

    let mut files: Vec<_> = std::fs::read_dir(src)
        .unwrap_or_else(|e| panic!("can't read {}: {e}", src.display()))
        .map(|e| e.expect("directory entry").path())
        .filter(|p| p.extension().is_some_and(|x| x == "rs"))
        .collect();
    files.sort();

    let mut sites = Vec::new();
    let mut problems = Vec::new();
    let (mut total_lines, mut unsafe_lines) = (0, 0);
    for path in &files {
        let name = format!("src/{}", path.file_name().unwrap().to_string_lossy());
        let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("can't read {name}: {e}"));
        let lines: Vec<&str> = text.lines().collect();
        total_lines += lines.len();
        let tokens = tokenize(&text);
        let mut inside = vec![false; lines.len() + 2];

        for (i, t) in tokens.iter().enumerate() {
            if t.text != "unsafe" { continue; }
            let next = |k: usize| tokens.get(i + k).map(|t| t.text.as_str()).unwrap_or("");
            let kind = match next(1) {
                "{" => Kind::Block,
                "impl" => Kind::Impl,
                "fn" => Kind::Fn,
                "trait" => Kind::Trait,
                "extern" if next(2) == "\"\"" && next(3) == "fn" => Kind::Fn,
                "(" => continue, // #[unsafe(...)], an attribute
                other => {
                    problems.push(format!("{name}:{}: `unsafe {other}` isn't something this check knows", t.line));
                    continue;
                }
            };
            if kind == Kind::Block {
                // The block's lines, up to the matching brace: how much of
                // the kernel is inside an unsafe block.
                let mut depth = 0;
                for u in &tokens[i + 1..] {
                    match u.text.as_str() {
                        "{" => depth += 1,
                        "}" => { depth -= 1; if depth == 0 {
                            for l in t.line..=u.line { inside[l] = true; }
                            break;
                        } }
                        _ => {}
                    }
                }
            }
            match justification(&lines, t.line, kind) {
                Ok((category, why)) => sites.push(Site { file: name.clone(), line: t.line, kind, category, why }),
                Err(e) => problems.push(format!("{name}:{}: unsafe {}: {e}", t.line, kind.name())),
            }
        }
        unsafe_lines += inside.iter().filter(|&&b| b).count();
    }

    if !problems.is_empty() {
        eprintln!("\nunsafe without a reason (build/unsafe_check.rs says what each needs):");
        for p in &problems { eprintln!("  {p}"); }
        eprintln!();
        std::process::exit(1);
    }

    let count = |k: Kind| sites.iter().filter(|s| s.kind == k).count();
    let mut out = String::new();
    let _ = writeln!(out, "Every unsafe in the FreyaOS kernel, and why it is sound.");
    let _ = writeln!(out, "Made by build/unsafe_check.rs from the SAFETY comments in kernel/src (Entry 30).");
    let _ = writeln!(out);
    let _ = writeln!(out, "{} unsafe blocks, {} unsafe functions, {} unsafe impls, {} unsafe traits in {} files.",
        count(Kind::Block), count(Kind::Fn), count(Kind::Impl), count(Kind::Trait),
        sites.iter().map(|s| &s.file).collect::<std::collections::BTreeSet<_>>().len());
    let _ = writeln!(out, "{unsafe_lines} of the kernel's {total_lines} lines are inside unsafe blocks.");
    let _ = writeln!(out);
    let _ = writeln!(out, "Blocks and impls by category:");
    for (c, what) in CATEGORIES {
        let n = sites.iter().filter(|s| s.category == *c).count();
        let _ = writeln!(out, "  {c:<7}{n:>4}  {what}");
    }
    let _ = writeln!(out);
    for s in &sites {
        let at = format!("{}:{}", s.file, s.line);
        let _ = writeln!(out, "{at:<20} {:<6} {:<7} {}", s.kind.name(), s.category, s.why);
    }
    std::fs::write(list, out).unwrap_or_else(|e| panic!("can't write {list}: {e}"));
}

/// The comment lines (and attributes) directly above line `line` (1-based),
/// nearest last.
fn above<'a>(lines: &[&'a str], line: usize) -> Vec<&'a str> {
    let mut run = Vec::new();
    let mut l = line - 1;
    while l > 0 {
        let t = lines[l - 1].trim();
        if t.starts_with("//") || t.starts_with("#[") {
            run.push(t);
            l -= 1;
        } else {
            break;
        }
    }
    run.reverse();
    run
}

fn justification(lines: &[&str], line: usize, kind: Kind) -> Result<(&'static str, String), String> {
    let run = above(lines, line);
    let text = |t: &str| t.trim_start_matches('/').trim().to_string();
    match kind {
        Kind::Block | Kind::Impl => {
            let at = run.iter().position(|t| text(t).starts_with("SAFETY:"))
                .ok_or("no `// SAFETY: [category] why` comment directly above it")?;
            let mut why = text(run[at])["SAFETY:".len()..].trim().to_string();
            for t in &run[at + 1..] {
                if !t.starts_with("//") { break; }
                why.push(' ');
                why.push_str(&text(t));
            }
            let rest = why.strip_prefix('[').ok_or("its SAFETY comment doesn't start with a [category]")?;
            let (c, why) = rest.split_once(']').ok_or("its SAFETY comment's [category] isn't closed")?;
            let category = CATEGORIES.iter().find(|(name, _)| *name == c)
                .ok_or_else(|| format!("[{c}] isn't a category (see CATEGORIES in build/unsafe_check.rs)"))?.0;
            let why = why.trim();
            if why.len() < 8 {
                return Err(format!("its SAFETY comment is too short to say why: {why:?}"));
            }
            Ok((category, why.to_string()))
        }
        Kind::Fn | Kind::Trait => {
            let docs: Vec<&str> = run.iter().filter(|t| t.starts_with("///")).copied().collect();
            let at = docs.iter().position(|t| text(t) == "# Safety")
                .ok_or("no `# Safety` section in its doc comment")?;
            let why: Vec<String> = docs[at + 1..].iter().map(|t| text(t))
                .take_while(|t| !t.starts_with("# ")).filter(|t| !t.is_empty()).collect();
            if why.is_empty() {
                return Err("its `# Safety` section is empty".into());
            }
            Ok(("", why.join(" ")))
        }
    }
}

struct Token { text: String, line: usize }

/// Words and punctuation, with comments, strings and character literals
/// skipped (each string becomes a single `""` token).
fn tokenize(src: &str) -> Vec<Token> {
    let c: Vec<char> = src.chars().collect();
    let mut tokens = Vec::new();
    let (mut i, mut line) = (0, 1);
    let at = |i: usize| c.get(i).copied().unwrap_or('\0');
    while i < c.len() {
        let ch = c[i];
        if ch == '\n' { line += 1; i += 1; continue; }
        if ch.is_whitespace() { i += 1; continue; }
        if ch == '/' && at(i + 1) == '/' {
            while i < c.len() && c[i] != '\n' { i += 1; }
            continue;
        }
        if ch == '/' && at(i + 1) == '*' {
            let mut depth = 0;
            while i < c.len() {
                if c[i] == '/' && at(i + 1) == '*' { depth += 1; i += 2; continue; }
                if c[i] == '*' && at(i + 1) == '/' { depth -= 1; i += 2; if depth == 0 { break; } continue; }
                if c[i] == '\n' { line += 1; }
                i += 1;
            }
            continue;
        }
        if ch == '"' {
            let start = line;
            i += 1;
            while i < c.len() && c[i] != '"' {
                if c[i] == '\\' { i += 1; }
                if at(i) == '\n' { line += 1; }
                i += 1;
            }
            i += 1;
            tokens.push(Token { text: "\"\"".into(), line: start });
            continue;
        }
        if ch == '\'' {
            if at(i + 1) == '\\' {
                // '\n', '\'', '\x41', '\u{1F600}': past the backslash and
                // the character after it, then to the closing quote.
                i += 3;
                while i < c.len() && c[i] != '\'' { i += 1; }
                i += 1;
                continue;
            }
            if at(i + 2) == '\'' { i += 3; continue; } // 'a'
            i += 1; // a lifetime or a label: its name lexes as a word
            continue;
        }
        if ch.is_alphabetic() || ch == '_' {
            let start = i;
            while i < c.len() && (c[i].is_alphanumeric() || c[i] == '_') { i += 1; }
            let word: String = c[start..i].iter().collect();
            // r"..", r#".."#, br".." (raw strings); b".." and b'.' are
            // handled by the quote rules once the prefix is skipped.
            if (word == "r" || word == "br") && (at(i) == '"' || (at(i) == '#' && (at(i + 1) == '"' || at(i + 1) == '#'))) {
                let mut hashes = 0;
                while at(i) == '#' { hashes += 1; i += 1; }
                let start_line = line;
                i += 1; // the opening quote
                loop {
                    if i >= c.len() { break; }
                    if c[i] == '\n' { line += 1; }
                    if c[i] == '"' && (1..=hashes).all(|k| at(i + k) == '#') { i += 1 + hashes; break; }
                    i += 1;
                }
                tokens.push(Token { text: "\"\"".into(), line: start_line });
                continue;
            }
            if word == "b" && at(i) == '\'' {
                // b'x' or b'\n'
                i += 1;
                if at(i) == '\\' { i += 1; }
                i += 1;
                while i < c.len() && c[i] != '\'' { i += 1; }
                i += 1;
                continue;
            }
            if word == "r" && at(i) == '#' { i += 1; continue; } // r#ident: the ident follows
            if word == "b" && at(i) == '"' { continue; }          // b"...": the string rule takes it
            tokens.push(Token { text: word, line });
            continue;
        }
        if ch.is_ascii_digit() {
            while i < c.len() && (c[i].is_alphanumeric() || c[i] == '_') { i += 1; }
            continue;
        }
        tokens.push(Token { text: ch.to_string(), line });
        i += 1;
    }
    tokens
}
