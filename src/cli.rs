use crate::mutation;
use crate::store::{Revision, Store};
use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};
use std::process::Command;

const USAGE: &str = "GET:\n  bedit -g FILE [REV]\n  bedit -g[b|d] FILE [REV]\n\nRESTORE:\n  bedit -r FILE [REV]\n  bedit -r[b|d] FILE [REV]\n\nOTHER:\n  bedit -s TERM\n  bedit -s FILE TERM\n  bedit -w FILE\n  bedit -ls [.|DIR|FILE|*]\n  bedit -ls[f|u|g] [.|DIR|FILE|*]\n  bedit -d FILE REV\n  bedit -d FILE REV\n  bedit -d FILE REV1 REV2\n  bedit -h [FILE]\n";

struct Output {
    color: bool,
}

impl Output {
    fn new() -> Self {
        let forced = env::var("BEDIT_COLOR")
            .ok()
            .is_some_and(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "yes" | "true"));
        Self {
            color: forced || io::stdout().is_terminal(),
        }
    }
    fn paint(&self, code: &str, text: impl AsRef<str>) -> String {
        if self.color {
            format!("\x1b[{code}m{}\x1b[0m", text.as_ref())
        } else {
            text.as_ref().to_owned()
        }
    }
    fn label(&self, text: impl AsRef<str>) -> String {
        self.paint("38;5;33", text)
    }
    fn path(&self, text: impl AsRef<str>) -> String {
        self.paint("36", text)
    }
    fn rev(&self, text: impl AsRef<str>) -> String {
        self.paint("35", text)
    }
    fn warn(&self, text: impl AsRef<str>) -> String {
        self.paint("33", text)
    }
    fn diff(&self, text: &str) -> String {
        if !self.color {
            return text.to_owned();
        }
        text.split_inclusive('\n')
            .map(|line| {
                let body = line.strip_suffix('\n').unwrap_or(line);
                let newline = if line.ends_with('\n') { "\n" } else { "" };
                let code = if body.starts_with('+') && !body.starts_with("+++") {
                    Some("32")
                } else if body.starts_with('-') && !body.starts_with("---") {
                    Some("31")
                } else if body.starts_with("@@")
                    || body.starts_with("---")
                    || body.starts_with("+++")
                {
                    Some("36")
                } else {
                    None
                };
                match code {
                    Some(c) => format!("{}{}", self.paint(c, body), newline),
                    None => line.to_owned(),
                }
            })
            .collect()
    }
}

pub fn main(mut args: Vec<String>) -> i32 {
    args.retain(|arg| arg != "--");
    let out = Output::new();
    let store = match Store::from_environment() {
        Ok(v) => v,
        Err(e) => return raw_error(&e.to_string(), 255),
    };
    let config = store.config();
    for path in [
        &config.access,
        &config.backups,
        &config.edits,
        &config.dirs,
        &config.tags,
    ] {
        if let Err(error) = config.create_dir_all(path) {
            return raw_error(&error.to_string(), 255);
        }
    }
    let Some(flag) = args.first().map(String::as_str) else {
        eprint!("{USAGE}");
        return 2;
    };
    let result = match flag {
        "--help" | "-?" => {
            print!("{USAGE}");
            Ok(())
        }
        "-h" | "-history" => history(&store, args.get(1), &out),
        "-w" | "-where" => where_cmd(&store, args.get(1), &out),
        "-s" | "-search" => search(&store, &args[1..], &out),
        "-d" | "-diff" | "-dif" => diff_cmd(&store, &args[1..], &out),
        "-tag" => tag(&store, &args[1..]),
        "-goto" => goto(&store, &args[1..]),
        f if f.starts_with("-ls") => listing(&store, f, &args[1..], &out),
        f if valid_restore_flag(f) => restore(&store, f, &args[1..]),
        f if valid_get_flag(f) => get(&store, f, &args[1..], &out),
        _ => Err(CliError::raw(USAGE, 2)),
    };
    match result {
        Ok(()) => 0,
        Err(e) => e.emit(&out),
    }
}

struct CliError {
    message: String,
    code: i32,
    stdout: bool,
    color_warn: bool,
}
impl CliError {
    fn raw(message: impl Into<String>, code: i32) -> Self {
        Self {
            message: message.into(),
            code,
            stdout: false,
            color_warn: false,
        }
    }
    fn local(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            code: 1,
            stdout: true,
            color_warn: true,
        }
    }
    fn emit(self, out: &Output) -> i32 {
        let text = if self.color_warn {
            out.warn(&self.message)
        } else {
            self.message
        };
        if self.stdout {
            println!("{text}");
        } else {
            eprint!("{text}");
            if !text.ends_with('\n') {
                eprintln!();
            }
        }
        self.code
    }
}
fn raw_error(message: &str, code: i32) -> i32 {
    eprintln!("{message}");
    code
}
fn revisions(store: &Store) -> Result<Vec<Revision>, CliError> {
    store
        .revisions()
        .map_err(|e| CliError::raw(e.to_string(), 255))
}
fn valid_get_flag(flag: &str) -> bool {
    flag.strip_prefix("-g")
        .is_some_and(|s| s.chars().all(|c| c == 'b' || c == 'd'))
}
fn valid_restore_flag(flag: &str) -> bool {
    flag.strip_prefix("-r")
        .is_some_and(|s| s.chars().all(|c| c == 'b' || c == 'd'))
}

fn canonical_candidate(query: &str) -> io::Result<PathBuf> {
    let path = Path::new(query);
    let full = if path.is_absolute() {
        path.to_path_buf()
    } else {
        env::current_dir()?.join(path)
    };
    let parent = full.parent().unwrap_or(Path::new("."));
    let parent = fs::canonicalize(parent).unwrap_or_else(|_| parent.to_path_buf());
    Ok(parent.join(full.file_name().unwrap_or_default()))
}

fn resolve<'a>(
    rows: &'a [Revision],
    query: &str,
    allow_missing_live: bool,
) -> Result<(PathBuf, Vec<&'a Revision>), CliError> {
    let full = canonical_candidate(query).map_err(|e| CliError::raw(e.to_string(), 255))?;
    let selected: Vec<_> = rows.iter().filter(|r| r.access.path == full).collect();
    if !selected.is_empty() && (allow_missing_live || full.exists()) {
        return Ok((full, selected));
    }
    if !full.exists() {
        let mut message = format!("bedit: There is no {query} in this directory. Either use fully qualified file names or goto the dir and try again.");
        let leaf = Path::new(query)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(query);
        let mut similar: Vec<_> = rows
            .iter()
            .filter(|r| r.leaf.contains(leaf) || leaf.contains(&r.leaf))
            .map(|r| r.access.path.clone())
            .collect();
        similar.sort();
        similar.dedup();
        if !similar.is_empty() {
            message.push_str("\n\nSimilar files in repo:\n");
            for p in similar {
                message.push_str(&format!("  {}\n", p.display()));
            }
            message.pop();
        }
        return Err(CliError::local(message));
    }
    Err(CliError::local(format!("bedit: File {query} does not exist in the bedit repo. To add it to the repo, open it in an editor.")))
}

fn tag(store: &Store, args: &[String]) -> Result<(), CliError> {
    const TAG_USAGE: &str = "usage: bedit -tag FILE TAG\n       bedit -tag FILE REV TAG\n";
    if args.len() < 2 {
        return Err(CliError::raw(TAG_USAGE, 255));
    }
    let rows = revisions(store)?;
    let (path, selected) = resolve(&rows, &args[0], true)?;
    let (number, words) = if args.len() >= 3 && args[1].chars().all(|c| c.is_ascii_digit()) {
        (args[1].parse::<u64>().unwrap_or(0), &args[2..])
    } else {
        (
            selected.iter().map(|r| r.number).max().unwrap_or(0),
            &args[1..],
        )
    };
    let text = words.join(" ").replace('\r', "");
    let text = text.trim();
    if text.is_empty() {
        return Err(CliError::raw(TAG_USAGE, 25));
    }
    let revision = selected
        .into_iter()
        .find(|r| r.number == number)
        .ok_or_else(|| CliError::raw("Revision not found\n", 2))?;
    mutation::write_tag(store, revision, text)
        .map_err(|_| CliError::raw("cannot write tag\n", 255))?;
    println!("bedit: tagged {} rev {}: {}", path.display(), number, text);
    Ok(())
}

fn restore(store: &Store, flag: &str, args: &[String]) -> Result<(), CliError> {
    if args.is_empty() || args.len() > 2 {
        return Err(CliError::raw(USAGE, 255));
    }
    let rows = revisions(store)?;
    let (path, selected) = resolve(&rows, &args[0], false)?;
    let number = match args.get(1) {
        Some(v) => v
            .parse::<u64>()
            .map_err(|_| CliError::raw("Revision not found\n", 255))?,
        None => selected.iter().map(|r| r.number).max().unwrap_or(0),
    };
    let revision = selected
        .iter()
        .find(|r| r.number == number)
        .ok_or_else(|| CliError::raw("Revision not found\n", 2))?;
    let suffix = &flag[2..];
    let want_backup = suffix.is_empty() || suffix.contains('b');
    let want_diff = suffix.contains('d');
    if want_backup {
        let editor = if want_diff {
            "restore-rendered"
        } else {
            "restore-backup"
        };
        let created = mutation::restore_revision(store, revision, want_diff, editor)
            .map_err(|_| CliError::raw("restore failed\n", 255))?;
        warn_created(&path, &created);
        println!(
            "restored {} from rev {} as new rev {}",
            path.display(),
            number,
            created.number
        );
        return Ok(());
    }
    let live = fs::read(&path).map_err(|_| CliError::raw("Target file not found\n", 255))?;
    let target = match (want_backup, want_diff) {
        (false, true) => restore_diff_target(revision, &live)?,
        _ => return Err(CliError::raw(USAGE, 255)),
    };
    preserve_if_needed(store, &path, &selected, &live)?;
    mutation::replace_live(&path, &target).map_err(|_| CliError::raw("restore failed\n", 255))?;
    let editor = "restore-diff";
    let created = publish_replaced_state(store, &path, editor, &live, &target)?;
    warn_created(&path, &created);
    println!(
        "restored {} from rev {} as new rev {}",
        path.display(),
        number,
        created.number
    );
    Ok(())
}

fn restore_diff_target(revision: &Revision, live: &[u8]) -> Result<Vec<u8>, CliError> {
    let diff = revision
        .diff
        .as_ref()
        .ok_or_else(|| CliError::raw("Diff not found\n", 2))?;
    let backup =
        fs::read(&revision.backup).map_err(|_| CliError::raw("Revision not found\n", 2))?;
    if live != backup {
        print!(
            "WARNING: current file differs from revision {} backup. Apply diff anyway? [y/N] ",
            revision.number
        );
        use std::io::Write;
        io::stdout().flush().ok();
        let mut answer = String::new();
        io::stdin().read_line(&mut answer).ok();
        if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
            return Err(CliError::raw("Aborted\n", 255));
        }
    }
    let temporary = revision
        .backup
        .parent()
        .unwrap()
        .join(format!(".rust-restore-diff-{}", std::process::id()));
    fs::write(&temporary, live).map_err(|_| CliError::raw("cannot apply diff\n", 255))?;
    let output = temporary.with_extension("out");
    let status = Command::new("patch")
        .args(["-s", "-N", "-o"])
        .arg(&output)
        .arg(&temporary)
        .arg(diff)
        .status();
    let data = match status {
        Ok(v) if v.success() => fs::read(&output),
        _ => Err(io::Error::other("patch failed")),
    };
    let _ = fs::remove_file(temporary);
    let _ = fs::remove_file(output);
    data.map_err(|_| CliError::raw("cannot apply diff\n", 255))
}

fn goto(store: &Store, args: &[String]) -> Result<(), CliError> {
    if args.len() < 2 {
        return Err(CliError::raw("usage: bedit -goto FILE TAG\n", 255));
    }
    let rows = revisions(store)?;
    let (path, selected) = resolve(&rows, &args[0], false)?;
    let needle = args[1..].join(" ");
    let revision = selected
        .iter()
        .find(|r| r.tag.as_deref() == Some(needle.as_str()))
        .ok_or_else(|| CliError::raw("Tag not found\n", 255))?;
    let created = mutation::restore_revision(store, revision, true, "goto")
        .map_err(|_| CliError::raw("cannot write target\n", 255))?;
    warn_created(&path, &created);
    println!(
        "bedit: restored {} from tag '{}' (rev {}) as new rev {}",
        path.display(),
        needle,
        revision.number,
        created.number
    );
    Ok(())
}

fn preserve_if_needed(
    store: &Store,
    path: &Path,
    selected: &[&Revision],
    live: &[u8],
) -> Result<(), CliError> {
    let latest = selected
        .iter()
        .max_by_key(|r| r.number)
        .ok_or_else(|| CliError::raw("Revision not found\n", 2))?;
    let represented = store
        .render(latest)
        .map_err(|_| CliError::raw("cannot render latest revision\n", 255))?;
    if represented == live {
        return Ok(());
    }
    let created = mutation::create_revision(store, path, "sync", &represented, live)
        .map_err(|e| CliError::raw(format!("cannot preserve live file: {e}\n"), 255))?;
    warn_created(path, &created);
    Ok(())
}

fn publish_replaced_state(
    store: &Store,
    path: &Path,
    editor: &str,
    prior_live: &[u8],
    target: &[u8],
) -> Result<mutation::CreatedRevision, CliError> {
    match mutation::create_revision(store, path, editor, prior_live, target) {
        Ok(created) => Ok(created),
        Err(error) => {
            let rollback = mutation::replace_live(path, prior_live);
            let message = match rollback {
                Ok(()) => format!("cannot preserve restored state: {error}; restored prior live file\n"),
                Err(rollback_error) => format!(
                    "cannot preserve restored state: {error}; cannot restore prior live file: {rollback_error}\n"
                ),
            };
            Err(CliError::raw(message, 255))
        }
    }
}

fn warn_fallback(path: &Path, number: u64, fallback: bool) {
    if fallback {
        eprintln!(
            "bedit: warning: diff generation failed; preserved {} as backup-only revision {}",
            path.display(),
            number
        );
    }
}

fn warn_created(path: &Path, created: &mutation::CreatedRevision) {
    warn_fallback(path, created.number, created.backup_only_fallback);
    if let Some(warning) = &created.mirror_warning {
        eprintln!("bedit: warning: {}: {warning}", path.display());
    }
}

fn short_stamp(epoch: u64) -> String {
    if epoch == 0 {
        return String::new();
    }
    let year = Command::new("date")
        .args(["+%Y"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .unwrap_or_default();
    let then_year = Command::new("date")
        .args(["-d", &format!("@{epoch}"), "+%Y"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .unwrap_or_default();
    let fmt = if year == then_year {
        "+%b %e %H:%M"
    } else {
        "+%b %e  %Y"
    };
    Command::new("date")
        .args(["-d", &format!("@{epoch}"), fmt])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim_end().to_owned())
        .unwrap_or_default()
}

fn paint_sync(
    store: &Store,
    rows: &[Revision],
    revision: &Revision,
    text: String,
    out: &Output,
) -> String {
    if !out.color {
        return text;
    }
    let latest = rows
        .iter()
        .filter(|r| r.access.path == revision.access.path)
        .map(|r| r.number)
        .max()
        .unwrap_or(0);
    if revision.number != latest {
        return out.paint("2", text);
    }
    let synced = store
        .render(revision)
        .ok()
        .and_then(|v| fs::read(&revision.access.path).ok().map(|live| live == v))
        .unwrap_or(false);
    let colored = out.paint(if synced { "32" } else { "31" }, text);
    out.paint("1", colored)
}

fn history(store: &Store, query: Option<&String>, out: &Output) -> Result<(), CliError> {
    let mut rows = revisions(store)?;
    if let Some(q) = query {
        if Path::new(q).is_absolute() {
            let p = canonical_candidate(q).unwrap();
            rows.retain(|r| r.access.path == p);
        } else {
            rows.retain(|r| &r.leaf == q);
        }
    }
    let local = query
        .filter(|q| !Path::new(q.as_str()).is_absolute())
        .and_then(|q| canonical_candidate(q).ok());
    rows.sort_by(|a, b| {
        let ag = local.as_ref().is_some_and(|p| &a.access.path != p);
        let bg = local.as_ref().is_some_and(|p| &b.access.path != p);
        ag.cmp(&bg)
            .then_with(|| b.access.epoch.cmp(&a.access.epoch))
            .then_with(|| a.access.path.cmp(&b.access.path))
            .then_with(|| b.number.cmp(&a.number))
    });
    let displays: Vec<String> = rows
        .iter()
        .map(|r| {
            if local.as_ref().is_some_and(|p| &r.access.path != p) {
                format!("other {}", r.access.path.display())
            } else {
                r.access.path.display().to_string()
            }
        })
        .collect();
    let fw = displays.iter().map(String::len).max().unwrap_or(4).max(4);
    let ww = rows
        .iter()
        .map(|r| short_stamp(r.access.epoch).len())
        .max()
        .unwrap_or(4)
        .max(4);
    let ew = rows
        .iter()
        .map(|r| r.access.editor.chars().take(4).count())
        .max()
        .unwrap_or(6)
        .max(6);
    let uw = rows.iter().map(|r| r.actor.len()).max().unwrap_or(4).max(4);
    let tw = rows
        .iter()
        .map(|r| r.tag.as_deref().unwrap_or("").len())
        .max()
        .unwrap_or(3)
        .max(3);
    println!(
        "{}  {}  {}  {}  {}  {}",
        out.label(format!("{:>3}", "REV")),
        out.label(format!("{:<fw$}", "FILE")),
        out.label(format!("{:<ww$}", "WHEN")),
        out.label(format!("{:<ew$}", "EDITOR")),
        out.label(format!("{:<uw$}", "USER")),
        out.label(format!("{:<tw$}", "TAG"))
    );
    let mut previous_primary = true;
    for (r, display) in rows.iter().zip(displays) {
        let primary = !display.starts_with("other ");
        if previous_primary
            && !primary
            && rows
                .iter()
                .any(|x| local.as_ref().is_some_and(|p| &x.access.path == p))
        {
            println!();
        }
        previous_primary = primary;
        let tag = r.tag.as_deref().unwrap_or("");
        let rev_text = paint_sync(store, &rows, r, format!("{:>3}", r.number), out);
        let file_text = paint_sync(store, &rows, r, format!("{display:<fw$}"), out);
        let tag_text = if tag.is_empty() {
            format!("{tag:<tw$}")
        } else {
            out.warn(format!("{tag:<tw$}"))
        };
        println!(
            "{}  {}  {:<ww$}  {:<ew$}  {:<uw$}  {}",
            rev_text,
            file_text,
            short_stamp(r.access.epoch),
            r.access.editor.chars().take(4).collect::<String>(),
            r.actor,
            tag_text
        );
    }
    Ok(())
}

fn get(store: &Store, flag: &str, args: &[String], out: &Output) -> Result<(), CliError> {
    let q = args.first().ok_or_else(|| CliError::raw(USAGE, 255))?;
    let rows = revisions(store)?;
    let (path, selected) = resolve(&rows, q, true)?;
    let latest = args.get(1).is_none();
    let rev = match args.get(1) {
        Some(v) => v
            .parse()
            .map_err(|_| CliError::raw("Revision not found\n", 255))?,
        None => selected.iter().map(|r| r.number).max().unwrap_or(0),
    };
    let r = selected
        .into_iter()
        .find(|r| r.number == rev)
        .ok_or_else(|| CliError::raw("Revision not found\n", 2))?;
    let mode = &flag[2..];
    let backup = mode.is_empty() || mode.contains('b');
    let diff = mode.contains('d');
    let (kind, data) = match (backup, diff) {
        (true, false) => (
            "backup",
            fs::read(&r.backup).map_err(|e| CliError::raw(e.to_string(), 255))?,
        ),
        (false, true) => {
            let p = r
                .diff
                .as_ref()
                .ok_or_else(|| CliError::raw("Diff not found\n", 2))?;
            (
                "diff",
                fs::read(p).map_err(|e| CliError::raw(e.to_string(), 2))?,
            )
        }
        (true, true) => (
            "rendered",
            store
                .render(r)
                .map_err(|e| CliError::raw(e.to_string(), 255))?,
        ),
        _ => ("", vec![]),
    };
    println!(
        "{} {}",
        out.label("FILE:"),
        out.path(path.display().to_string())
    );
    print!("{} {}", out.label("REV:"), out.rev(rev.to_string()));
    if latest {
        print!(" {}", out.warn("(latest)"));
    }
    println!("\n{} {kind}\n", out.label("TYPE:"));
    let text = String::from_utf8_lossy(&data);
    if kind == "diff" {
        print!("{}", out.diff(&text));
    } else {
        print!("{text}");
    }
    Ok(())
}

fn where_cmd(store: &Store, query: Option<&String>, out: &Output) -> Result<(), CliError> {
    let q = query.ok_or_else(|| CliError::raw("usage: bedit -w FILE\n", 2))?;
    let rows = revisions(store)?;
    let mut paths: Vec<_> = if Path::new(q).is_absolute() {
        let p = canonical_candidate(q).unwrap();
        rows.into_iter()
            .filter(|r| r.access.path == p)
            .map(|r| r.access.path)
            .collect()
    } else {
        rows.into_iter()
            .filter(|r| &r.leaf == q)
            .map(|r| r.access.path)
            .collect()
    };
    paths.sort();
    paths.dedup();
    for p in paths {
        println!("{}", out.path(p.display().to_string()));
    }
    Ok(())
}

fn search(store: &Store, args: &[String], out: &Output) -> Result<(), CliError> {
    if !(args.len() == 1 || args.len() == 2) {
        return Err(CliError::raw(USAGE, 2));
    }
    let (file, needle) = if args.len() == 1 {
        (None, &args[0])
    } else {
        (Some(&args[0]), &args[1])
    };
    if needle.is_empty() {
        return Err(CliError::raw(
            "usage: bedit -s TERM\n       bedit -s FILE TERM\n",
            2,
        ));
    }
    let mut rows = revisions(store)?;
    if let Some(q) = file {
        if Path::new(q).is_absolute() {
            let p = canonical_candidate(q).unwrap();
            rows.retain(|r| r.access.path == p)
        } else {
            rows.retain(|r| &r.leaf == q)
        }
    }
    rows.sort_by(|a, b| {
        a.access
            .path
            .cmp(&b.access.path)
            .then(a.number.cmp(&b.number))
    });
    type SearchHits = Vec<(usize, String)>;
    type SearchGroups<'a> = Vec<(u64, &'a str, SearchHits)>;
    let mut grouped: BTreeMap<PathBuf, SearchGroups<'_>> = BTreeMap::new();
    for r in &rows {
        for (kind, path) in [("backup", Some(&r.backup)), ("diff", r.diff.as_ref())] {
            if let Some(path) = path {
                let text =
                    fs::read_to_string(path).map_err(|e| CliError::raw(e.to_string(), 255))?;
                let hits: Vec<_> = text
                    .split('\n')
                    .enumerate()
                    .filter(|(_, l)| l.contains(needle))
                    .map(|(n, l)| (n + 1, l.to_owned()))
                    .collect();
                if !hits.is_empty() {
                    grouped
                        .entry(r.access.path.clone())
                        .or_default()
                        .push((r.number, kind, hits));
                }
            }
        }
    }
    for (path, entries) in grouped {
        println!(
            "{} {}",
            out.label("FILE:"),
            out.path(path.display().to_string())
        );
        for (rev, kind, hits) in entries {
            println!(
                "  {} {} {}",
                out.label("rev"),
                out.rev(rev.to_string()),
                out.label(format!("{kind}:"))
            );
            for (n, line) in hits {
                let painted = if out.color {
                    line.replace(needle, &out.paint("32", needle))
                } else {
                    line
                };
                println!("    {}: {painted}", out.rev(n.to_string()));
            }
        }
        println!();
    }
    Ok(())
}

fn rendered_temp(store: &Store, r: &Revision, label: &str) -> Result<PathBuf, CliError> {
    let data = store
        .render(r)
        .map_err(|e| CliError::raw(e.to_string(), 255))?;
    let p = store
        .config()
        .root
        .join(format!(".diff-{label}-{}-{}", std::process::id(), r.leaf));
    fs::write(&p, data).map_err(|e| CliError::raw(e.to_string(), 255))?;
    Ok(p)
}
fn diff_cmd(store: &Store, args: &[String], out: &Output) -> Result<(), CliError> {
    if args.len() < 2 || args.len() > 3 {
        return Err(CliError::raw(
            "usage: bedit -d FILE REV\n       bedit -d FILE REV1 REV2\n",
            2,
        ));
    }
    let rows = revisions(store)?;
    let (path, selected) = resolve(&rows, &args[0], true)?;
    let r1n = args[1].parse::<u64>().map_err(|_| {
        CliError::raw(
            "usage: bedit -d FILE REV\n       bedit -d FILE REV1 REV2\n",
            2,
        )
    })?;
    let r1 = selected
        .iter()
        .find(|r| r.number == r1n)
        .ok_or_else(|| CliError::raw("Revision not found\n", 2))?;
    let left = rendered_temp(store, r1, "left")?;
    let (right, label) = if let Some(v) = args.get(2) {
        let n = v.parse::<u64>().map_err(|_| {
            CliError::raw(
                "usage: bedit -d FILE REV\n       bedit -d FILE REV1 REV2\n",
                2,
            )
        })?;
        let r = selected
            .iter()
            .find(|r| r.number == n)
            .ok_or_else(|| CliError::raw("Revision not found\n", 2))?;
        (
            rendered_temp(store, r, "right")?,
            format!("{}@{n}", path.display()),
        )
    } else {
        (path.clone(), format!("{}@live", path.display()))
    };
    let output = Command::new("diff")
        .args(["-u", "--"])
        .arg(&left)
        .arg(&right)
        .output()
        .map_err(|e| CliError::raw(e.to_string(), 255))?;
    if output.status.code().is_some_and(|code| code > 1) {
        let _ = fs::remove_file(&left);
        if args.len() == 3 {
            let _ = fs::remove_file(&right);
        }
        return Err(CliError::raw("diff failed\n", 2));
    }
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    if let Some(pos) = text.find('\n') {
        text.replace_range(..pos, &format!("--- {}@{r1n}", path.display()));
        if let Some(start) = text[pos + 1..].find("+++ ").map(|x| x + pos + 1) {
            if let Some(end) = text[start..].find('\n').map(|x| x + start) {
                text.replace_range(start..end, &format!("+++ {label}"));
            }
        }
    }
    let _ = fs::remove_file(left);
    if args.len() == 3 {
        let _ = fs::remove_file(right);
    }
    print!("{}", out.diff(&text));
    Ok(())
}

#[derive(Clone)]
struct ListItem {
    path: PathBuf,
    display: String,
    rev: u64,
    kind: &'static str,
    epoch: u64,
    editor: String,
    actor: String,
}

fn recorded_actor(revision: Option<&Revision>) -> &str {
    revision.map_or("", |revision| revision.actor.as_str())
}

fn listing(store: &Store, flag: &str, args: &[String], out: &Output) -> Result<(), CliError> {
    let scope = if args.len() > 1 {
        "*"
    } else {
        args.first().map(String::as_str).unwrap_or(".")
    };
    let cwd = env::current_dir()
        .and_then(fs::canonicalize)
        .map_err(|e| CliError::raw(e.to_string(), 255))?;
    let global = scope == "*";
    let scope_path = if global {
        None
    } else if scope == "." {
        Some(cwd.clone())
    } else {
        Some(fs::canonicalize(scope).unwrap_or_else(|_| cwd.join(scope)))
    };
    let rows = revisions(store)?;
    let mut items = Vec::new();
    for e in store
        .index_entries()
        .map_err(|e| CliError::raw(e.to_string(), 255))?
    {
        let path = e.branch.join(&e.leaf);
        let include = global
            || scope_path.as_ref().is_some_and(|p| {
                if p.is_dir() {
                    path.parent() == Some(p.as_path())
                } else {
                    &path == p
                }
            });
        if !include {
            continue;
        }
        let row = rows
            .iter()
            .find(|r| r.access.path == path && r.number == e.revision);
        let display = if global {
            path.display().to_string()
        } else {
            e.leaf.clone()
        };
        items.push(ListItem {
            path,
            display,
            rev: e.revision,
            kind: match e.kind {
                'a' => "access",
                'b' => "backup",
                _ => "diff",
            },
            epoch: row.map_or(0, |r| r.access.epoch),
            editor: row.map_or(String::new(), |r| r.access.editor.chars().take(4).collect()),
            actor: recorded_actor(row).to_owned(),
        });
    }
    match flag {
        "-lsf" => items.sort_by(|a, b| {
            a.display
                .cmp(&b.display)
                .then(b.rev.cmp(&a.rev))
                .then(a.kind.cmp(b.kind))
        }),
        "-lsu" => items.sort_by(|a, b| {
            a.actor
                .cmp(&b.actor)
                .then(b.epoch.cmp(&a.epoch))
                .then(a.display.cmp(&b.display))
        }),
        _ => items.sort_by(|a, b| {
            b.epoch
                .cmp(&a.epoch)
                .then(a.display.cmp(&b.display))
                .then(a.kind.cmp(b.kind))
        }),
    }
    if flag == "-lsg" {
        let mut groups: BTreeMap<PathBuf, Vec<ListItem>> = BTreeMap::new();
        for i in items {
            groups.entry(i.path.clone()).or_default().push(i)
        }
        for (_, mut group) in groups {
            println!("{} {}", out.label("FILE:"), out.path(&group[0].display));
            group.sort_by(|a, b| b.rev.cmp(&a.rev).then(a.kind.cmp(b.kind)));
            for i in group {
                let r = rows
                    .iter()
                    .find(|r| r.access.path == i.path && r.number == i.rev);
                let rev = match r {
                    Some(r) => paint_sync(store, &rows, r, format!("{:>3}", i.rev), out),
                    None => format!("{:>3}", i.rev),
                };
                let kind = match r {
                    Some(r) => paint_sync(store, &rows, r, format!("{:<7}", i.kind), out),
                    None => format!("{:<7}", i.kind),
                };
                println!(
                    "  {}  {}  {:<12}  {:<4}  {}",
                    rev,
                    kind,
                    short_stamp(i.epoch),
                    i.editor,
                    i.actor
                );
            }
            println!();
        }
        return Ok(());
    }
    let fw = items
        .iter()
        .map(|i| i.display.len())
        .max()
        .unwrap_or(4)
        .max(4);
    let uw = items
        .iter()
        .map(|i| i.actor.len())
        .max()
        .unwrap_or(4)
        .max(4);
    println!(
        "{}  {}  {}  {}  {}  {}",
        out.label(format!("{:<fw$}", "FILE")),
        out.label(format!("{:>3}", "REV")),
        out.label(format!("{:<7}", "TYPE")),
        out.label(format!("{:<12}", "WHEN")),
        out.label(format!("{:<4}", "EDIT")),
        out.label(format!("{:<uw$}", "USER"))
    );
    for i in items {
        let r = rows
            .iter()
            .find(|r| r.access.path == i.path && r.number == i.rev);
        let file = match r {
            Some(r) => paint_sync(store, &rows, r, format!("{:<fw$}", i.display), out),
            None => format!("{:<fw$}", i.display),
        };
        let rev = match r {
            Some(r) => paint_sync(store, &rows, r, format!("{:>3}", i.rev), out),
            None => format!("{:>3}", i.rev),
        };
        let kind = match r {
            Some(r) => paint_sync(store, &rows, r, format!("{:<7}", i.kind), out),
            None => format!("{:<7}", i.kind),
        };
        println!(
            "{}  {}  {}  {:<12}  {:<4}  {:<uw$}",
            file,
            rev,
            kind,
            short_stamp(i.epoch),
            i.editor,
            i.actor
        );
    }
    Ok(())
}

#[cfg(test)]
mod actor_presentation_tests {
    use super::*;

    #[test]
    fn listing_uses_stored_actor_and_never_substitutes_current_user() {
        let revision = Revision {
            key: "tmp".into(),
            leaf: "a.txt".into(),
            number: 2,
            access: crate::store::AccessRecord {
                epoch: 1,
                stamp: "1970-01-01 00:00:01 +0000".into(),
                path: "/tmp/a.txt".into(),
                editor: "vi".into(),
            },
            backup: "/tmp/backup".into(),
            diff: None,
            tag: None,
            actor: "faf".into(),
        };

        assert_eq!(recorded_actor(Some(&revision)), "faf");
        assert_eq!(recorded_actor(None), "");
    }

    #[test]
    fn exact_stored_path_resolves_when_live_file_is_on_another_host() {
        let path = PathBuf::from(format!(
            "/bedit-cross-platform-missing-{}/portable.txt",
            std::process::id()
        ));
        assert!(!path.exists());
        let revision = Revision {
            key: "portable".into(),
            leaf: "portable.txt".into(),
            number: 1,
            access: crate::store::AccessRecord {
                epoch: 1,
                stamp: "1970-01-01 00:00:01 +0000".into(),
                path: path.clone(),
                editor: "ed".into(),
            },
            backup: "/tmp/backup".into(),
            diff: None,
            tag: None,
            actor: "tester".into(),
        };

        let rows = [revision];
        let Ok((resolved, selected)) = resolve(&rows, path.to_str().unwrap(), true) else {
            panic!("exact stored path did not resolve");
        };
        assert_eq!(resolved, path);
        assert_eq!(selected.len(), 1);
    }

    #[test]
    fn exact_stored_path_does_not_resolve_for_mutation_when_live_file_is_missing() {
        let path = PathBuf::from(format!(
            "/bedit-local-mutation-missing-{}/portable.txt",
            std::process::id()
        ));
        assert!(!path.exists());
        let revision = Revision {
            key: "portable".into(),
            leaf: "portable.txt".into(),
            number: 1,
            access: crate::store::AccessRecord {
                epoch: 1,
                stamp: "1970-01-01 00:00:01 +0000".into(),
                path: path.clone(),
                editor: "ed".into(),
            },
            backup: "/tmp/backup".into(),
            diff: None,
            tag: None,
            actor: "tester".into(),
        };

        let rows = [revision];
        let error = resolve(&rows, path.to_str().unwrap(), false).unwrap_err();
        assert_eq!(error.code, 1);
    }
}
