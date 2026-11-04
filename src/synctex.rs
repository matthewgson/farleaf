//! SyncTeX inverse search: from a spot on a page to the line of LaTeX that
//! made it, and on to the editor.
//!
//! LaTeX run with `-synctex=1` writes FILE.synctex.gz beside FILE.pdf; TeX
//! Live's `synctex edit` reads it. The editor command is
//! $FARLEAF_SYNCTEX_EDITOR, with {file}, {line} and {column} in it. It is
//! split into words as a shell would (quotes work), the placeholders are
//! filled in inside each word, and it runs without a shell, so a path with
//! spaces or quotes in it stays one argument.
//!
//! (The other way, from the source to the PDF, needs nothing here: an editor
//! runs `synctex view` and opens farleaf with `-p PAGE:Y`, or types `:PAGE:Y`
//! into the farleaf that is open.)

use crate::worker::{self, Event};
use std::ffi::OsString;
use std::os::fd::RawFd;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::Sender;

pub const EDITOR_VAR: &str = "FARLEAF_SYNCTEX_EDITOR";

/// The SyncTeX file LaTeX wrote beside `pdf`, if there is one.
pub fn data_file(pdf: &Path) -> Option<PathBuf> {
    ["synctex.gz", "synctex"]
        .iter()
        .map(|ext| pdf.with_extension(ext))
        .find(|p| p.is_file())
}

/// A line of source, as `synctex edit` gives it.
#[derive(Debug, PartialEq)]
pub struct Source {
    pub file: PathBuf,
    pub line: u32,
    /// From 1; 1 when SyncTeX does not know (pdfTeX never records columns).
    pub column: u32,
}

/// Looks up the source of the spot `x`, `y` (points from the top-left
/// corner) on page `page` (from 0) of `pdf`, and opens it in the editor, on
/// a thread of its own. What happened comes back as Event::Synctex: the
/// place found (as soon as it is found), and an error if the editor fails.
pub fn inverse(pdf: PathBuf, page: usize, x: f32, y: f32, tx: Sender<Event>, wake: RawFd) {
    let report = move |m: Result<String, String>| worker::send(&tx, wake, Event::Synctex(m));
    let spawned = std::thread::Builder::new().name("synctex".into()).spawn(move || {
        let src = match lookup(&pdf, page, x, y) {
            Ok(s) => s,
            Err(e) => return report(Err(e)),
        };
        let name = src.file.file_name().unwrap_or(src.file.as_os_str()).to_string_lossy();
        let at = format!("{name}:{}", src.line);
        match std::env::var(EDITOR_VAR) {
            Ok(cmd) if !cmd.trim().is_empty() => {
                report(Ok(at));
                if let Err(e) = open(&cmd, &src) {
                    report(Err(e));
                }
            }
            _ => report(Ok(format!("{at} (set {EDITOR_VAR} to go there in your editor)"))),
        }
    });
    if let Err(e) = spawned {
        debug_log!("synctex thread: {e}");
    }
}

/// Runs `synctex edit` for the spot.
fn lookup(pdf: &Path, page: usize, x: f32, y: f32) -> Result<Source, String> {
    let pdf = std::fs::canonicalize(pdf).unwrap_or_else(|_| pdf.to_path_buf());
    if data_file(&pdf).is_none() {
        return Err("no SyncTeX data beside this PDF (run LaTeX with -synctex=1)".into());
    }
    let mut spot = OsString::from(format!("{}:{x:.2}:{y:.2}:", page + 1));
    spot.push(pdf.as_os_str());
    let out = detached(Command::new("synctex").arg("edit").arg("-o").arg(spot))
        .stdout(Stdio::piped())
        .output()
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => "inverse search needs the synctex command (TeX Live)".to_string(),
            _ => format!("synctex: {e}"),
        })?;
    let dir = pdf.parent().unwrap_or(Path::new("."));
    let src = parse_edit(&String::from_utf8_lossy(&out.stdout), dir);
    debug_log!("synctex edit p{} {x:.1},{y:.1}: {src:?}", page + 1);
    src.ok_or_else(|| "SyncTeX has no source line for this spot".into())
}

/// The first result in what `synctex edit` printed. Its input paths are
/// relative to the PDF's directory unless absolute, and often have a "./"
/// in them; editors compare paths, so they are made canonical.
fn parse_edit(out: &str, dir: &Path) -> Option<Source> {
    let mut file = None;
    let mut line = None;
    let mut column = 1;
    for l in out.lines() {
        if let Some(v) = l.strip_prefix("Input:") {
            if file.is_some() {
                break; // the next result
            }
            file = Some(v.to_string());
        } else if let Some(v) = l.strip_prefix("Line:") {
            line = line.or(v.trim().parse::<u32>().ok());
        } else if let Some(v) = l.strip_prefix("Column:") {
            column = v.trim().parse::<i64>().ok().filter(|&c| c > 0).map_or(1, |c| c as u32);
        }
    }
    // components() drops the "." parts, for a file that is gone.
    let file: PathBuf = dir.join(file?).components().collect();
    let file = std::fs::canonicalize(&file).unwrap_or(file);
    Some(Source {
        file,
        line: line.filter(|&l| l > 0)?,
        column,
    })
}

/// Runs the editor command for `src` and waits for it.
fn open(cmd: &str, src: &Source) -> Result<(), String> {
    let words = split_words(cmd).ok_or_else(|| format!("{EDITOR_VAR}: a quote is not closed"))?;
    let (file, line, column) = (src.file.to_string_lossy(), src.line.to_string(), src.column.to_string());
    let vars = [("{file}", &*file), ("{line}", &*line), ("{column}", &*column)];
    let argv: Vec<String> = words.iter().map(|w| fill(w, &vars)).collect();
    let Some((prog, args)) = argv.split_first() else {
        return Err(format!("{EDITOR_VAR} is empty"));
    };
    debug_log!("synctex editor: {argv:?}");
    let out = detached(Command::new(prog).args(args))
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("{prog}: {e}"))?;
    if out.status.success() {
        return Ok(());
    }
    let err = String::from_utf8_lossy(&out.stderr);
    let why = err
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map_or_else(|| out.status.to_string(), str::to_string);
    Err(format!("{prog}: {why}"))
}

/// A child with nothing of farleaf's terminal: no input or output on it,
/// and a session of its own, so it cannot read keys or write over the
/// pages even by opening /dev/tty.
fn detached(c: &mut Command) -> &mut Command {
    c.stdin(Stdio::null()).stderr(Stdio::null());
    // SAFETY: setsid is async-signal-safe, as the child between fork and
    // exec requires.
    unsafe {
        c.pre_exec(|| {
            libc::setsid();
            Ok(())
        })
    }
}

/// `s` with each placeholder replaced, in one pass (a file name with
/// "{line}" in it stays as it is).
fn fill(s: &str, vars: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    'scan: while !rest.is_empty() {
        for (k, v) in vars {
            if let Some(after) = rest.strip_prefix(k) {
                out.push_str(v);
                rest = after;
                continue 'scan;
            }
        }
        let c = rest.chars().next().unwrap_or_default();
        out.push(c);
        rest = &rest[c.len_utf8()..];
    }
    out
}

/// Splits a command line into words the way a shell does, for the parts a
/// command needs: blanks between words, '...' as it is, "..." with \" \\ \$
/// and \` escaped, and \ before any other character. None if a quote is not
/// closed.
fn split_words(s: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match c {
            '\'' => loop {
                in_word = true;
                match chars.next()? {
                    '\'' => break,
                    c => cur.push(c),
                }
            },
            '"' => loop {
                in_word = true;
                match chars.next()? {
                    '"' => break,
                    '\\' => match chars.next()? {
                        c @ ('"' | '\\' | '$' | '`') => cur.push(c),
                        c => {
                            cur.push('\\');
                            cur.push(c);
                        }
                    },
                    c => cur.push(c),
                }
            },
            '\\' => {
                in_word = true;
                cur.push(chars.next()?);
            }
            c if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut cur));
                    in_word = false;
                }
            }
            c => {
                in_word = true;
                cur.push(c);
            }
        }
    }
    if in_word {
        words.push(cur);
    }
    Some(words)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words() {
        let w = |s| split_words(s).unwrap();
        assert_eq!(
            w(r#"nvim --headless -c "VimtexInverseSearch {line} '{file}'""#),
            ["nvim", "--headless", "-c", "VimtexInverseSearch {line} '{file}'"]
        );
        assert_eq!(w(r#"  a\ b 'c d'"e\"f\n" ''  "#), ["a b", "c de\"f\\n", ""]);
        assert_eq!(w(""), Vec::<String>::new());
        assert_eq!(split_words("a 'b"), None);
        assert_eq!(split_words("a \"b\\"), None);
        assert_eq!(split_words("a\\"), None);
    }

    #[test]
    fn placeholders() {
        let vars = [("{file}", "/a b/{line}.tex"), ("{line}", "12"), ("{column}", "1")];
        assert_eq!(fill("{file}:{line}:{column}", &vars), "/a b/{line}.tex:12:1");
        assert_eq!(fill("+{line} {x} é", &vars), "+12 {x} é");
    }

    #[test]
    fn edit_output() {
        let out = "This is SyncTeX command line utility, version 1.5\nSyncTeX result begin\n\
                   Output:/d/main.pdf\nInput:/nonexistent-dir/./main.tex\nLine:63\nColumn:-1\nOffset:0\n\
                   Context:\nSyncTeX result end\n";
        assert_eq!(
            parse_edit(out, Path::new("/d")),
            Some(Source {
                file: "/nonexistent-dir/main.tex".into(),
                line: 63,
                column: 1
            })
        );
        // Relative inputs are the PDF directory's; the first result wins.
        let two = "Input:chap.tex\nLine:7\nColumn:4\nInput:/x.tex\nLine:9\n";
        assert_eq!(
            parse_edit(two, Path::new("/nonexistent-dir")),
            Some(Source {
                file: "/nonexistent-dir/chap.tex".into(),
                line: 7,
                column: 4
            })
        );
        assert_eq!(
            parse_edit("This is SyncTeX command line utility\n", Path::new("/")),
            None
        );
        assert_eq!(parse_edit("Input:/a.tex\nLine:0\n", Path::new("/")), None);
    }
}
