#[cfg(test)]
use crate::config::Config;
use crate::editor::{self, EditorFamily};
use crate::mutation;
use crate::store::Store;
use std::collections::BTreeMap;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::collections::HashMap;
use std::env;
#[cfg(target_os = "linux")]
use std::ffi::OsStr;
use std::fs;
use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};
#[cfg(any(target_os = "macos", test))]
use std::process::Stdio;
use std::process::{Command, ExitStatus};
use std::sync::atomic::{AtomicI32, Ordering};
use std::thread;
use std::time::{Duration, Instant};
#[cfg(any(target_os = "macos", test))]
use std::{ffi::OsString, io::Read};

pub struct WrapperSpec {
    pub wrapper: &'static str,
    pub editor: &'static str,
}

static CHILD_PID: AtomicI32 = AtomicI32::new(0);
static RECEIVED_SIGNAL: AtomicI32 = AtomicI32::new(0);

extern "C" fn forward_signal(signal: libc::c_int) {
    RECEIVED_SIGNAL.store(signal, Ordering::SeqCst);
    let child = CHILD_PID.load(Ordering::SeqCst);
    if child > 0 {
        unsafe {
            libc::kill(child, signal);
        }
    }
}

struct Track {
    path: PathBuf,
    leaf: String,
    baseline: Vec<u8>,
    observed: Option<Vec<u8>>,
    published: bool,
}

#[derive(Default)]
struct SessionResults {
    files: BTreeMap<PathBuf, FileSessionResult>,
    mirror_warnings: Vec<String>,
}

struct FileSessionResult {
    leaf: String,
    revisions: usize,
    last_revision: u64,
    last_diff: String,
    fallback_revisions: Vec<u64>,
}

impl SessionResults {
    fn captured(
        &mut self,
        track: &Track,
        revision: u64,
        diff: String,
        fallback: bool,
        mirror_warning: Option<String>,
    ) {
        let result = self
            .files
            .entry(track.path.clone())
            .or_insert_with(|| FileSessionResult {
                leaf: track.leaf.clone(),
                revisions: 0,
                last_revision: revision,
                last_diff: String::new(),
                fallback_revisions: Vec::new(),
            });
        result.revisions += 1;
        result.last_revision = revision;
        result.last_diff = diff;
        if fallback {
            result.fallback_revisions.push(revision);
        }
        if let Some(warning) = mirror_warning {
            self.mirror_warnings
                .push(format!("{}: {warning}", track.leaf));
        }
    }
    fn render(&self) {
        for result in self.files.values() {
            println!(
                "{}: {} revision{} captured",
                result.leaf,
                result.revisions,
                if result.revisions == 1 { "" } else { "s" }
            );
            if !result.fallback_revisions.is_empty() {
                let revisions = result
                    .fallback_revisions
                    .iter()
                    .map(u64::to_string)
                    .collect::<Vec<_>>()
                    .join(", ");
                eprintln!(
                    "bedit: warning: diff failed for {}; preserved backup-only revision{} {}",
                    result.leaf,
                    if result.fallback_revisions.len() == 1 {
                        ""
                    } else {
                        "s"
                    },
                    revisions
                );
            }
            if !result.last_diff.is_empty() {
                println!();
                print!("{}", result.last_diff);
                if !result.last_diff.ends_with('\n') {
                    println!();
                }
            }
        }
        for warning in &self.mirror_warnings {
            eprintln!("bedit: warning: {warning}");
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum DiscoveryRejection {
    Deleted,
    NotAbsolute,
    NotRegular,
    PseudoFilesystem,
    RepositoryInternal,
    AlreadyTracked,
    VimAsset,
    VimTemporary,
    Excluded,
    InvalidPath,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DiscoveryProfile {
    Vim,
    Neovim,
    Nano,
    Emacs,
}

trait OpenFileDiscovery {
    fn discover_open_files(&self, editor_pid: u32) -> io::Result<Vec<PathBuf>>;
}

#[cfg(target_os = "linux")]
struct ProcDiscovery;

#[cfg(any(target_os = "linux", target_os = "macos"))]
struct DirectoryEventDiscovery {
    #[cfg(target_os = "linux")]
    fd: i32,
    #[cfg(target_os = "linux")]
    watches: HashMap<i32, PathBuf>,
    #[cfg(target_os = "macos")]
    watches: HashMap<PathBuf, std::collections::HashSet<PathBuf>>,
    #[cfg(target_os = "macos")]
    pending_initial: Vec<DirectoryEvent>,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
struct DirectoryEvent {
    path: PathBuf,
    created: bool,
}

fn coalesce_directory_events(events: Vec<DirectoryEvent>) -> Vec<DirectoryEvent> {
    let mut coalesced = BTreeMap::new();
    for event in events {
        coalesced
            .entry(event.path)
            .and_modify(|created| *created |= event.created)
            .or_insert(event.created);
    }
    coalesced
        .into_iter()
        .map(|(path, created)| DirectoryEvent { path, created })
        .collect()
}

#[cfg(target_os = "linux")]
impl DirectoryEventDiscovery {
    fn new(tracks: &[Track]) -> io::Result<Self> {
        let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut discovery = Self {
            fd,
            watches: HashMap::new(),
        };
        if let Ok(cwd) = env::current_dir() {
            discovery.watch(&cwd);
        }
        for track in tracks {
            if let Some(parent) = track.path.parent() {
                discovery.watch(parent);
            }
        }
        Ok(discovery)
    }

    fn watch(&mut self, directory: &Path) {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        let directory = fs::canonicalize(directory).unwrap_or_else(|_| directory.to_path_buf());
        if self.watches.values().any(|watched| watched == &directory) {
            return;
        }
        let Ok(path) = CString::new(directory.as_os_str().as_bytes()) else {
            return;
        };
        let mask = libc::IN_OPEN | libc::IN_CREATE | libc::IN_MOVED_TO;
        let wd = unsafe { libc::inotify_add_watch(self.fd, path.as_ptr(), mask) };
        if wd >= 0 {
            self.watches.insert(wd, directory);
        }
    }

    fn drain(&mut self) -> io::Result<Vec<DirectoryEvent>> {
        use std::ffi::CStr;
        use std::os::unix::ffi::OsStrExt;
        let mut result = Vec::new();
        let mut buffer = [0_u8; 8192];
        loop {
            let count = unsafe { libc::read(self.fd, buffer.as_mut_ptr().cast(), buffer.len()) };
            if count < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::WouldBlock {
                    break;
                }
                return Err(error);
            }
            if count == 0 {
                break;
            }
            let mut offset = 0;
            while offset + std::mem::size_of::<libc::inotify_event>() <= count as usize {
                let event = unsafe {
                    std::ptr::read_unaligned(
                        buffer.as_ptr().add(offset).cast::<libc::inotify_event>(),
                    )
                };
                let size = std::mem::size_of::<libc::inotify_event>() + event.len as usize;
                if offset + size > count as usize {
                    break;
                }
                if event.len > 0 && event.mask & libc::IN_ISDIR == 0 {
                    let name = unsafe {
                        CStr::from_ptr(
                            buffer
                                .as_ptr()
                                .add(offset + std::mem::size_of::<libc::inotify_event>())
                                .cast(),
                        )
                    };
                    if let Some(directory) = self.watches.get(&event.wd) {
                        result.push(DirectoryEvent {
                            path: directory.join(OsStr::from_bytes(name.to_bytes())),
                            created: event.mask & (libc::IN_CREATE | libc::IN_MOVED_TO) != 0,
                        });
                    }
                }
                offset += size;
            }
        }
        Ok(coalesce_directory_events(result))
    }
}

#[cfg(target_os = "macos")]
impl DirectoryEventDiscovery {
    fn new(tracks: &[Track]) -> io::Result<Self> {
        let mut discovery = Self {
            watches: HashMap::new(),
            pending_initial: Vec::new(),
        };
        if let Ok(cwd) = env::current_dir() {
            discovery.watch(&cwd);
        }
        for track in tracks {
            if let Some(parent) = track.path.parent() {
                discovery.watch(parent);
            }
        }
        Ok(discovery)
    }

    fn watch(&mut self, directory: &Path) {
        let directory = fs::canonicalize(directory).unwrap_or_else(|_| directory.to_path_buf());
        if self.watches.contains_key(&directory) {
            return;
        }
        let known = directory_entries(&directory).unwrap_or_default();
        self.pending_initial
            .extend(known.iter().map(|name| DirectoryEvent {
                path: directory.join(name),
                created: false,
            }));
        self.watches.insert(directory, known);
    }

    fn drain(&mut self) -> io::Result<Vec<DirectoryEvent>> {
        let mut result = std::mem::take(&mut self.pending_initial);
        for (directory, known) in &mut self.watches {
            let current = directory_entries(directory)?;
            result.extend(current.difference(known).map(|name| DirectoryEvent {
                path: directory.join(name),
                created: true,
            }));
            *known = current;
        }
        Ok(coalesce_directory_events(result))
    }

    fn knew(&self, target: &Path) -> Option<bool> {
        let parent = target.parent()?;
        let parent = fs::canonicalize(parent).unwrap_or_else(|_| parent.to_path_buf());
        let name = target.file_name()?;
        self.watches
            .get(&parent)
            .map(|known| known.contains(Path::new(name)))
    }
}

#[cfg(target_os = "macos")]
fn directory_entries(directory: &Path) -> io::Result<std::collections::HashSet<PathBuf>> {
    fs::read_dir(directory)?
        .map(|entry| entry.map(|entry| PathBuf::from(entry.file_name())))
        .collect()
}

#[cfg(target_os = "linux")]
impl Drop for DirectoryEventDiscovery {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.fd);
        }
    }
}

#[cfg(target_os = "linux")]
impl OpenFileDiscovery for ProcDiscovery {
    fn discover_open_files(&self, editor_pid: u32) -> io::Result<Vec<PathBuf>> {
        let mut paths = Vec::new();
        for entry in fs::read_dir(format!("/proc/{editor_pid}/fd"))? {
            let Ok(entry) = entry else { continue };
            if !discoverable_fd_name(&entry.file_name().to_string_lossy()) {
                continue;
            }
            if let Ok(path) = fs::read_link(entry.path()) {
                paths.push(path);
            }
        }
        Ok(paths)
    }
}

#[cfg(any(target_os = "macos", test))]
struct LsofDiscovery {
    executable: PathBuf,
    timeout: Duration,
}

#[cfg(any(target_os = "macos", test))]
impl LsofDiscovery {
    fn new(executable: PathBuf, timeout: Duration) -> Self {
        Self {
            executable,
            timeout,
        }
    }
}

#[cfg(any(target_os = "macos", test))]
impl OpenFileDiscovery for LsofDiscovery {
    fn discover_open_files(&self, editor_pid: u32) -> io::Result<Vec<PathBuf>> {
        use std::os::unix::process::CommandExt;

        let mut command = Command::new(&self.executable);
        command
            .arg("-Fn")
            .arg("-p")
            .arg(editor_pid.to_string())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        unsafe {
            command.pre_exec(|| {
                if libc::setpgid(0, 0) == 0 {
                    Ok(())
                } else {
                    Err(io::Error::last_os_error())
                }
            });
        }
        let mut child = command.spawn()?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("lsof stdout unavailable"))?;
        let reader = thread::spawn(move || {
            let mut bytes = Vec::new();
            let mut stdout = stdout;
            stdout.read_to_end(&mut bytes).map(|_| bytes)
        });
        let deadline = Instant::now() + self.timeout;
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if Instant::now() >= deadline {
                unsafe {
                    libc::kill(-(child.id() as i32), libc::SIGKILL);
                }
                let _ = child.wait();
                let _ = reader.join();
                return Err(io::Error::new(io::ErrorKind::TimedOut, "lsof timed out"));
            }
            thread::sleep(Duration::from_millis(5));
        };
        let output = reader
            .join()
            .map_err(|_| io::Error::other("lsof output reader failed"))??;
        if !status.success() {
            return Err(io::Error::other(format!("lsof exited with {status}")));
        }
        Ok(parse_lsof_paths(&output))
    }
}

struct TerminalState(Option<libc::termios>);

impl TerminalState {
    fn capture(interactive: bool) -> Self {
        if !interactive {
            return Self(None);
        }
        let mut state = std::mem::MaybeUninit::<libc::termios>::uninit();
        let result = unsafe { libc::tcgetattr(libc::STDIN_FILENO, state.as_mut_ptr()) };
        if result == 0 {
            Self(Some(unsafe { state.assume_init() }))
        } else {
            Self(None)
        }
    }

    fn restore(&self) {
        if let Some(state) = &self.0 {
            unsafe {
                libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, state);
            }
        }
    }
}

impl Drop for TerminalState {
    fn drop(&mut self) {
        self.restore();
    }
}

pub fn main(spec: WrapperSpec, args: Vec<String>) -> i32 {
    match run(spec, args) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("{error}");
            255
        }
    }
}

fn run(spec: WrapperSpec, args: Vec<String>) -> io::Result<i32> {
    let store = Store::from_environment()?;
    let config = store.config();
    for path in [
        &config.access,
        &config.backups,
        &config.edits,
        &config.dirs,
        &config.tags,
    ] {
        config.create_dir_all(path)?;
    }
    let transparent_alias = env::var("BEDIT_EDITOR_ALIAS").ok();
    let editor_path = if let Some(alias) = transparent_alias.as_deref() {
        let entry = editor::alias(alias)
            .filter(|entry| entry.supported)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("bedit: unsupported editor alias: {alias}"),
                )
            })?;
        debug_assert_eq!(entry.strategy, editor::ProcessStrategy::SpawnedEditor);
        let path = env::var_os("PATH").unwrap_or_default();
        let shim_dir = env::var_os("BEDIT_SHIM_DIR").map(PathBuf::from);
        editor::resolve_executable(alias, &path, shim_dir.as_deref())?
    } else {
        match env::var("BEDIT_EDITOR").or_else(|_| env::var("EDITOR")) {
            Ok(editor) => PathBuf::from(editor),
            Err(_) => editor::resolve_executable(
                spec.editor,
                &env::var_os("PATH").unwrap_or_default(),
                None,
            )?,
        }
    };
    let editor = editor_path.to_string_lossy().into_owned();
    let editor_identity = transparent_alias.as_deref().unwrap_or(spec.editor);
    let mut tracks = collect_tracks(&store, &spec, &args)?;
    let interactive = io::stdin().is_terminal() && io::stdout().is_terminal();
    let terminal = TerminalState::capture(interactive);

    install_signal_forwarding();
    RECEIVED_SIGNAL.store(0, Ordering::SeqCst);
    let mut child = Command::new(&editor).args(&args).spawn()?;
    let editor_pid = child.id();
    CHILD_PID.store(editor_pid as i32, Ordering::SeqCst);
    let interval = poll_interval();
    let family = transparent_alias
        .as_deref()
        .and_then(editor::alias)
        .map(|entry| entry.family);
    let discovery_profile = cfg!(target_os = "linux")
        .then(|| {
            family
                .map(family_discovery_profile)
                .unwrap_or_else(|| discovery_profile(spec.wrapper))
        })
        .flatten()
        .or_else(|| {
            cfg!(target_os = "macos")
                .then(|| discovery_profile(spec.wrapper))
                .flatten()
        });
    let discovery = discovery_profile.and_then(platform_discovery);
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    let mut event_discovery =
        discovery_profile.and_then(|_| DirectoryEventDiscovery::new(&tracks).ok());
    let discovery_interval = discovery_profile.map(|_| discovery_interval());
    let session_interval = discovery_interval.map_or(interval, |value| value.min(interval));
    let home = env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    let discovery_roots =
        discovery_profile.map_or_else(Vec::new, |profile| platform_discovery_roots(profile, &home));
    let mut last_discovery = Instant::now()
        .checked_sub(discovery_interval.unwrap_or_default())
        .unwrap_or_else(Instant::now);
    let mut results = SessionResults::default();
    let status = 'session: loop {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        if let (Some(profile), Some(events)) = (discovery_profile, event_discovery.as_mut()) {
            if let Ok(found) = events.drain() {
                for event in found {
                    if let Some(path) = add_discovered_target(
                        &store,
                        profile,
                        &home,
                        &discovery_roots,
                        &mut tracks,
                        &event.path,
                        event.created,
                    ) {
                        if let Some(parent) = path.parent() {
                            events.watch(parent);
                        }
                    }
                }
            }
        }
        if let (Some(profile), Some(discovery)) = (discovery_profile, discovery.as_deref()) {
            if discovery_interval.is_some_and(|value| last_discovery.elapsed() >= value) {
                #[cfg(target_os = "macos")]
                let created = |target: &Path| {
                    event_discovery
                        .as_ref()
                        .and_then(|events| events.knew(target))
                        == Some(false)
                };
                #[cfg(not(target_os = "macos"))]
                let created = |_target: &Path| false;
                if let Ok(targets) = discovery.discover_open_files(editor_pid) {
                    for target in targets {
                        let is_created = created(&target);
                        let _ = add_discovered_target(
                            &store,
                            profile,
                            &home,
                            &discovery_roots,
                            &mut tracks,
                            &target,
                            is_created,
                        );
                    }
                }
                last_discovery = Instant::now();
            }
        }
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) => {}
            Err(error) => break Err(error),
        }
        for track in &mut tracks {
            if let Err(error) = poll_track(
                &store,
                track,
                editor_identity,
                false,
                interval,
                &mut results,
            ) {
                break 'session Err(error);
            }
        }
        thread::sleep(session_interval);
    };

    let status = match status {
        Ok(status) => status,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            CHILD_PID.store(0, Ordering::SeqCst);
            return Err(error);
        }
    };

    CHILD_PID.store(0, Ordering::SeqCst);
    terminal.restore();
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    if let (Some(profile), Some(events)) = (discovery_profile, event_discovery.as_mut()) {
        if let Ok(found) = events.drain() {
            for event in found {
                let _ = add_discovered_target(
                    &store,
                    profile,
                    &home,
                    &discovery_roots,
                    &mut tracks,
                    &event.path,
                    event.created,
                );
            }
        }
    }
    for track in &mut tracks {
        poll_track(&store, track, editor_identity, true, interval, &mut results)?;
    }
    if interactive {
        banner();
    }
    results.render();
    let signal = RECEIVED_SIGNAL.swap(0, Ordering::SeqCst);
    if signal > 0 {
        Ok(128 + signal)
    } else {
        Ok(editor_exit_code(status))
    }
}

fn family_discovery_profile(family: EditorFamily) -> Option<DiscoveryProfile> {
    match family {
        EditorFamily::Vim => Some(DiscoveryProfile::Vim),
        EditorFamily::Neovim => Some(DiscoveryProfile::Neovim),
        EditorFamily::Nano => Some(DiscoveryProfile::Nano),
        EditorFamily::Emacs => Some(DiscoveryProfile::Emacs),
        EditorFamily::Ed
        | EditorFamily::Micro
        | EditorFamily::Joe
        | EditorFamily::Jed
        | EditorFamily::McEdit => None,
    }
}

fn install_signal_forwarding() {
    unsafe {
        let handler = forward_signal as *const () as libc::sighandler_t;
        libc::signal(libc::SIGINT, handler);
        libc::signal(libc::SIGTERM, handler);
        libc::signal(libc::SIGHUP, handler);
    }
}

fn collect_tracks(store: &Store, spec: &WrapperSpec, args: &[String]) -> io::Result<Vec<Track>> {
    let mut result = Vec::new();
    let mut consume_next = false;
    for arg in args {
        if consume_next {
            consume_next = false;
            continue;
        }
        if arg == "--" || arg.starts_with('+') {
            continue;
        }
        if option_takes_value(spec, arg) {
            consume_next = true;
            continue;
        }
        if arg.starts_with('-') {
            continue;
        }
        let path = absolute_local(arg)?;
        let leaf = path
            .file_name()
            .and_then(|v| v.to_str())
            .unwrap_or_default()
            .to_owned();
        if path.is_dir() || excluded(store, &path, &leaf) {
            continue;
        }
        if !path.exists() {
            fs::File::create(&path)?;
        }
        if !path.is_file() {
            continue;
        }
        let baseline = fs::read(&path)?;
        result.push(Track {
            path,
            leaf,
            observed: Some(baseline.clone()),
            baseline,
            published: false,
        });
    }
    Ok(result)
}

fn poll_track(
    store: &Store,
    track: &mut Track,
    editor: &str,
    final_flush: bool,
    interval: Duration,
    results: &mut SessionResults,
) -> io::Result<()> {
    poll_track_after_first_read(store, track, editor, final_flush, interval, results, || {})
}

fn poll_track_after_first_read(
    store: &Store,
    track: &mut Track,
    editor: &str,
    final_flush: bool,
    interval: Duration,
    results: &mut SessionResults,
    after_first_read: impl FnOnce(),
) -> io::Result<()> {
    let Ok(first) = fs::read(&track.path) else {
        return Ok(());
    };
    after_first_read();
    if !final_flush && track.observed.as_ref() == Some(&first) {
        return Ok(());
    }
    trace_session(track, "observed", &first);
    let current = if final_flush {
        first
    } else {
        thread::sleep(interval);
        let Ok(second) = fs::read(&track.path) else {
            return Ok(());
        };
        if second != first {
            trace_session(track, "unstable", &second);
            return Ok(());
        }
        second
    };
    if current != track.baseline {
        trace_session(track, "publish-start", &current);
        let diff = diff_tail(store, track, &current);
        let created =
            mutation::create_revision(store, &track.path, editor, &track.baseline, &current)?;
        results.captured(
            track,
            created.number,
            diff,
            created.backup_only_fallback,
            created.mirror_warning,
        );
        track.baseline = current;
        track.observed = Some(track.baseline.clone());
        track.published = true;
        trace_session(track, "publish-complete", &track.baseline);
        mark_published(track, created.number);
    } else {
        track.observed = Some(current);
    }
    Ok(())
}

#[cfg(debug_assertions)]
fn mark_published(track: &Track, number: u64) {
    let Some(marker) = env::var_os("BEDIT_PUBLICATION_MARKER") else {
        return;
    };
    use std::io::Write;
    if let Ok(mut file) = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(marker)
    {
        let _ = writeln!(file, "{}\t{number}", track.path.display());
    }
}

#[cfg(not(debug_assertions))]
fn mark_published(_track: &Track, _number: u64) {}

#[cfg(debug_assertions)]
fn trace_session(track: &Track, event: &str, content: &[u8]) {
    let Some(path) = env::var_os("BEDIT_SESSION_TRACE") else {
        return;
    };
    use std::hash::{Hash, Hasher};
    use std::io::Write;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    content.hash(&mut hasher);
    if let Ok(mut trace) = fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(
            trace,
            "pid={} event={} path={} len={} hash={:016x}",
            std::process::id(),
            event,
            track.path.display(),
            content.len(),
            hasher.finish()
        );
    }
}

#[cfg(not(debug_assertions))]
fn trace_session(_track: &Track, _event: &str, _content: &[u8]) {}

fn diff_tail(store: &Store, track: &Track, current: &[u8]) -> String {
    let root = &store.config().root;
    let left = root.join(format!(".wrapper-diff-left-{}", std::process::id()));
    let right = root.join(format!(".wrapper-diff-right-{}", std::process::id()));
    if fs::write(&left, &track.baseline).is_err() || fs::write(&right, current).is_err() {
        return String::new();
    }
    let mut result = String::new();
    if let Ok(output) = Command::new("diff")
        .args(["-u", "--"])
        .arg(&left)
        .arg(&right)
        .output()
    {
        if matches!(output.status.code(), Some(0 | 1)) {
            let text = String::from_utf8_lossy(&output.stdout);
            let lines: Vec<_> = text.lines().collect();
            let start = lines.len().saturating_sub(store.config().diff_tail_lines);
            for line in &lines[start..] {
                result.push_str(line);
                result.push('\n');
            }
        }
    }
    let _ = fs::remove_file(left);
    let _ = fs::remove_file(right);
    result
}

fn option_takes_value(spec: &WrapperSpec, arg: &str) -> bool {
    matches!(
        arg,
        "-t" | "-T" | "-u" | "-U" | "-i" | "-p" | "-c" | "-S" | "-o" | "-O"
    ) || (spec.wrapper == "bemacs" && matches!(arg, "--eval" | "--execute"))
}

fn absolute_local(arg: &str) -> io::Result<PathBuf> {
    let path = Path::new(arg);
    let full = if path.is_absolute() {
        path.to_path_buf()
    } else {
        env::current_dir()?.join(path)
    };
    let parent = full.parent().unwrap_or(Path::new("."));
    let parent = fs::canonicalize(parent).unwrap_or_else(|_| parent.to_path_buf());
    Ok(parent.join(full.file_name().unwrap_or_default()))
}

fn excluded(store: &Store, path: &Path, leaf: &str) -> bool {
    if path.starts_with(&store.config().root) {
        return true;
    }
    store
        .config()
        .exclude
        .split(',')
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .any(|pattern| wildcard(pattern, &path.to_string_lossy()) || wildcard(pattern, leaf))
}

fn add_discovered_target(
    store: &Store,
    profile: DiscoveryProfile,
    home: &Path,
    runtime_roots: &[PathBuf],
    tracks: &mut Vec<Track>,
    target: &Path,
    created: bool,
) -> Option<PathBuf> {
    let metadata = fs::metadata(target).ok()?;
    if !metadata.is_file() {
        return None;
    }
    let canonical_target = fs::canonicalize(target).ok()?;
    if let Some(track) = tracks
        .iter_mut()
        .find(|track| fs::canonicalize(&track.path).ok().as_ref() == Some(&canonical_target))
    {
        if created && !track.published {
            track.baseline.clear();
            track.observed = None;
        }
        return None;
    }
    let tracked: Vec<_> = tracks
        .iter()
        .filter_map(|track| fs::canonicalize(&track.path).ok())
        .collect();
    let path = profiled_discovery_candidate(
        store,
        profile,
        home,
        runtime_roots,
        &tracked,
        target,
        &metadata,
    )
    .ok()?;
    let current = fs::read(&path).ok()?;
    let baseline = if created { Vec::new() } else { current.clone() };
    let leaf = path.file_name()?.to_str()?.to_owned();
    tracks.push(Track {
        path: path.clone(),
        leaf,
        observed: Some(baseline.clone()),
        baseline,
        published: false,
    });
    mark_discovered(&path);
    Some(path)
}

fn mark_discovered(path: &Path) {
    #[cfg(debug_assertions)]
    {
        let expected = env::var_os("BEDIT_DISCOVERY_EXPECT")
            .map(PathBuf::from)
            .map(|path| fs::canonicalize(&path).unwrap_or(path));
        if expected.as_ref().is_none_or(|expected| expected == path) {
            if let Some(marker) = env::var_os("BEDIT_DISCOVERY_MARKER") {
                use std::io::Write;
                if let Ok(mut file) = fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(marker)
                {
                    let _ = writeln!(file, "{}", path.display());
                }
            }
        }
    }
    #[cfg(not(debug_assertions))]
    let _ = path;
}

#[cfg(any(target_os = "linux", test))]
fn discoverable_fd_name(name: &str) -> bool {
    !matches!(name, "0" | "1" | "2")
}

#[cfg(any(target_os = "macos", test))]
fn parse_lsof_paths(output: &[u8]) -> Vec<PathBuf> {
    use std::os::unix::ffi::OsStringExt;

    let mut paths = Vec::new();
    let mut accept_name = false;
    for line in output.split(|byte| *byte == b'\n') {
        let Some((&field, value)) = line.split_first() else {
            continue;
        };
        match field {
            b'f' => {
                accept_name = std::str::from_utf8(value)
                    .ok()
                    .and_then(|value| value.parse::<u32>().ok())
                    .is_some_and(|fd| fd > 2);
            }
            b'n' if accept_name && value.starts_with(b"/") => {
                let path = PathBuf::from(OsString::from_vec(value.to_vec()));
                if !paths.contains(&path) {
                    paths.push(path);
                }
                accept_name = false;
            }
            b'p' => accept_name = false,
            _ => {}
        }
    }
    paths
}

fn platform_discovery(_profile: DiscoveryProfile) -> Option<Box<dyn OpenFileDiscovery>> {
    #[cfg(target_os = "linux")]
    {
        return Some(Box::new(ProcDiscovery));
    }
    #[cfg(target_os = "macos")]
    {
        let executable =
            env::var_os("BEDIT_LSOF").map_or_else(|| PathBuf::from("lsof"), PathBuf::from);
        return Some(Box::new(LsofDiscovery::new(
            executable,
            Duration::from_secs(2),
        )));
    }
    #[allow(unreachable_code)]
    None
}

#[cfg(test)]
fn discovery_candidate(
    store: &Store,
    home: &Path,
    vim_roots: &[PathBuf],
    tracked: &[PathBuf],
    fd_target: &Path,
    metadata: &fs::Metadata,
) -> Result<PathBuf, DiscoveryRejection> {
    profiled_discovery_candidate(
        store,
        DiscoveryProfile::Vim,
        home,
        vim_roots,
        tracked,
        fd_target,
        metadata,
    )
}

fn profiled_discovery_candidate(
    store: &Store,
    profile: DiscoveryProfile,
    home: &Path,
    runtime_roots: &[PathBuf],
    tracked: &[PathBuf],
    fd_target: &Path,
    metadata: &fs::Metadata,
) -> Result<PathBuf, DiscoveryRejection> {
    let target_text = fd_target.to_string_lossy();
    if target_text.ends_with(" (deleted)") {
        return Err(DiscoveryRejection::Deleted);
    }
    if !fd_target.is_absolute() {
        return Err(DiscoveryRejection::NotAbsolute);
    }
    if !metadata.is_file() {
        return Err(DiscoveryRejection::NotRegular);
    }
    let path = fs::canonicalize(fd_target).map_err(|_| DiscoveryRejection::InvalidPath)?;
    if ["/proc", "/sys", "/dev", "/run"]
        .iter()
        .any(|root| path.starts_with(root))
    {
        return Err(DiscoveryRejection::PseudoFilesystem);
    }
    if cfg!(target_os = "macos") && macos_system_asset(&path) {
        return Err(DiscoveryRejection::PseudoFilesystem);
    }
    let config = store.config();
    if [
        &config.root,
        &config.access,
        &config.backups,
        &config.edits,
        &config.dirs,
        &config.tags,
    ]
    .iter()
    .filter_map(|root| fs::canonicalize(root).ok())
    .any(|root| path.starts_with(root))
    {
        return Err(DiscoveryRejection::RepositoryInternal);
    }
    if tracked.iter().any(|tracked| tracked == &path) {
        return Err(DiscoveryRejection::AlreadyTracked);
    }
    let leaf = path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or(DiscoveryRejection::InvalidPath)?;
    let canonical_home = fs::canonicalize(home).unwrap_or_else(|_| home.to_path_buf());
    let canonical_roots: Vec<_> = runtime_roots
        .iter()
        .map(|root| fs::canonicalize(root).unwrap_or_else(|_| root.clone()))
        .collect();
    if editor_asset(profile, &path, leaf, &canonical_home, &canonical_roots) {
        return Err(DiscoveryRejection::VimAsset);
    }
    if editor_temporary(profile, leaf) {
        return Err(DiscoveryRejection::VimTemporary);
    }
    if excluded(store, &path, leaf) {
        return Err(DiscoveryRejection::Excluded);
    }
    Ok(path)
}

fn discovery_profile(wrapper: &str) -> Option<DiscoveryProfile> {
    match wrapper {
        "bvi" => Some(DiscoveryProfile::Vim),
        "bnvim" => Some(DiscoveryProfile::Neovim),
        "bnano" => Some(DiscoveryProfile::Nano),
        "bemacs" => Some(DiscoveryProfile::Emacs),
        _ => None,
    }
}

fn editor_asset(
    profile: DiscoveryProfile,
    path: &Path,
    leaf: &str,
    home: &Path,
    runtime_roots: &[PathBuf],
) -> bool {
    match profile {
        DiscoveryProfile::Vim => vim_asset(path, leaf, home, runtime_roots),
        DiscoveryProfile::Neovim => neovim_asset(path, leaf, home, runtime_roots),
        DiscoveryProfile::Nano => {
            matches!(leaf, "nanorc" | ".nanorc")
                || path.starts_with(home.join(".config/nano"))
                || runtime_roots.iter().any(|root| path.starts_with(root))
        }
        DiscoveryProfile::Emacs => {
            matches!(leaf, ".emacs" | "early-init.el" | "init.el")
                || path.starts_with(home.join(".emacs.d"))
                || path.starts_with(home.join(".config/emacs"))
                || path.starts_with(home.join(".local/share/emacs"))
                || runtime_roots.iter().any(|root| path.starts_with(root))
        }
    }
}

fn editor_temporary(profile: DiscoveryProfile, leaf: &str) -> bool {
    let common = vim_temporary(leaf);
    match profile {
        DiscoveryProfile::Vim | DiscoveryProfile::Neovim | DiscoveryProfile::Nano => common,
        DiscoveryProfile::Emacs => {
            common || leaf.starts_with(".#") || (leaf.starts_with('#') && leaf.ends_with('#'))
        }
    }
}

fn vim_asset(path: &Path, leaf: &str, home: &Path, vim_roots: &[PathBuf]) -> bool {
    const RUNTIME_FILES: &[&str] = &[
        "vimrc",
        ".vimrc",
        "syntax.vim",
        "synload.vim",
        "syncolor.vim",
        "filetype.vim",
        "netrwPlugin.vim",
        ".viminfo",
    ];
    if RUNTIME_FILES.contains(&leaf) || leaf.ends_with(".vim") {
        return true;
    }
    let roots = [
        home.join(".vim"),
        home.join(".config/vim"),
        home.join(".local/share/vim"),
        PathBuf::from("/etc/vim"),
        PathBuf::from("/usr/share/vim"),
        PathBuf::from("/usr/local/share/vim"),
    ];
    roots
        .iter()
        .chain(vim_roots)
        .any(|root| path.starts_with(root))
}

fn neovim_asset(path: &Path, leaf: &str, home: &Path, runtime_roots: &[PathBuf]) -> bool {
    const RUNTIME_FILES: &[&str] = &[
        "vimrc",
        ".vimrc",
        "syntax.vim",
        "synload.vim",
        "syncolor.vim",
        "filetype.vim",
        "netrwPlugin.vim",
        "shada.main",
        "main.shada",
    ];
    if RUNTIME_FILES.contains(&leaf) {
        return true;
    }
    let roots = [
        home.join(".config/nvim"),
        home.join(".local/share/nvim"),
        home.join(".cache/nvim"),
        home.join(".local/state/nvim"),
        PathBuf::from("/etc/xdg/nvim"),
        PathBuf::from("/usr/share/nvim"),
        PathBuf::from("/usr/local/share/nvim"),
    ];
    roots
        .iter()
        .chain(runtime_roots)
        .any(|root| path.starts_with(root))
}

fn vim_temporary(leaf: &str) -> bool {
    let lower = leaf.to_ascii_lowercase();
    let swap = lower.rsplit_once(".sw").is_some_and(|(_, suffix)| {
        suffix.len() == 1 && suffix.bytes().all(|c| c.is_ascii_lowercase())
    });
    swap || leaf.ends_with('~')
        || lower.ends_with(".tmp")
        || lower.ends_with(".temp")
        || lower.ends_with(".lock")
        || leaf.starts_with(".#")
        || leaf.starts_with(".tmp")
}

fn discovery_roots(profile: DiscoveryProfile, home: &Path) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    match profile {
        DiscoveryProfile::Vim => {
            if let Some(root) = env::var_os("VIMRUNTIME") {
                roots.push(PathBuf::from(root));
            }
            roots.push(home.join(".vim"));
            roots.push(home.join(".config/vim"));
        }
        DiscoveryProfile::Neovim => {
            if let Some(root) = env::var_os("VIMRUNTIME") {
                roots.push(PathBuf::from(root));
            }
            roots.push(home.join(".config/nvim"));
            roots.push(home.join(".local/share/nvim"));
            roots.push(home.join(".cache/nvim"));
            roots.push(home.join(".local/state/nvim"));
            roots.push(PathBuf::from("/usr/share/nvim"));
            roots.push(PathBuf::from("/usr/local/share/nvim"));
        }
        DiscoveryProfile::Nano => {
            roots.push(PathBuf::from("/usr/share/nano"));
            roots.push(PathBuf::from("/usr/local/share/nano"));
        }
        DiscoveryProfile::Emacs => {
            roots.push(PathBuf::from("/usr/share/emacs"));
            roots.push(PathBuf::from("/usr/local/share/emacs"));
            roots.push(PathBuf::from("/usr/share/emacs/site-lisp"));
            roots.push(PathBuf::from("/usr/local/share/emacs/site-lisp"));
        }
    }
    roots
}

fn macos_discovery_roots(profile: DiscoveryProfile, home: &Path) -> Vec<PathBuf> {
    let mut roots = discovery_roots(profile, home);
    match profile {
        DiscoveryProfile::Vim => {
            roots.push(PathBuf::from("/Applications/MacVim.app/Contents"));
            roots.push(PathBuf::from("/Applications/Vim.app/Contents"));
        }
        DiscoveryProfile::Neovim => {}
        DiscoveryProfile::Nano => {
            roots.push(PathBuf::from("/opt/homebrew/share/nano"));
            roots.push(PathBuf::from("/usr/local/share/nano"));
        }
        DiscoveryProfile::Emacs => {
            roots.push(PathBuf::from("/Applications/Emacs.app/Contents"));
            roots.push(PathBuf::from("/Applications/Aquamacs.app/Contents"));
            roots.push(PathBuf::from("/Library/Application Support/Emacs"));
        }
    }
    roots
}

fn macos_system_asset(path: &Path) -> bool {
    let text = path.to_string_lossy();
    path.starts_with("/System/Library")
        || path.starts_with("/usr/lib")
        || path.starts_with("/private/var/run")
        || ((path.starts_with("/private/var/folders") || path.starts_with("/var/folders"))
            && text
                .split_once("/T/")
                .is_some_and(|(_, relative)| !relative.contains('/')))
}

fn platform_discovery_roots(profile: DiscoveryProfile, home: &Path) -> Vec<PathBuf> {
    if cfg!(target_os = "macos") {
        macos_discovery_roots(profile, home)
    } else {
        discovery_roots(profile, home)
    }
}

fn wildcard(pattern: &str, value: &str) -> bool {
    if let Some((left, right)) = pattern.split_once('*') {
        value.starts_with(left) && value.ends_with(right)
    } else {
        pattern == value
    }
}

fn poll_interval() -> Duration {
    env::var("BEDIT_POLL_SECS")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .filter(|v| v.is_finite() && *v >= 0.0)
        .map(Duration::from_secs_f64)
        .unwrap_or(Duration::from_millis(20))
}

fn discovery_interval() -> Duration {
    let configured = env::var("BEDIT_DISCOVERY_SECS").ok();
    if cfg!(target_os = "macos") {
        macos_discovery_interval(configured.as_deref())
    } else {
        parse_discovery_interval(configured.as_deref()).unwrap_or(Duration::from_millis(5))
    }
}

fn macos_discovery_interval(configured: Option<&str>) -> Duration {
    parse_discovery_interval(configured).unwrap_or(Duration::from_millis(300))
}

fn parse_discovery_interval(configured: Option<&str>) -> Option<Duration> {
    configured
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|value| value.is_finite() && *value > 0.0)
        .map(Duration::from_secs_f64)
}

fn banner() {
    println!("\x1b[1;36mBedit — edits backed up by Bedit\x1b[0m");
}

fn editor_exit_code(status: ExitStatus) -> i32 {
    status.code().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    fn discovery_store(base: &Path) -> Store {
        Store::new(Config {
            root: base.join("store"),
            access: base.join("store/access"),
            backups: base.join("store/backups"),
            edits: base.join("store/edits"),
            dirs: base.join("store/dirs"),
            tags: base.join("store/tags"),
            actors: base.join("store/actors"),
            actor: "tester".into(),
            history_owner: "tester".into(),
            ownership: None,
            keep_backup_if_no_edit: true,
            diff_tail_lines: 20,
            exclude: "*.log".to_owned(),
        })
    }

    #[test]
    fn target_option_values_match_active_wrappers() {
        let vi = WrapperSpec {
            wrapper: "bvi",
            editor: "vi",
        };
        let emacs = WrapperSpec {
            wrapper: "bemacs",
            editor: "emacs",
        };
        assert!(option_takes_value(&vi, "-c"));
        assert!(option_takes_value(&vi, "-O"));
        assert!(!option_takes_value(&vi, "-x"));
        assert!(option_takes_value(&emacs, "--eval"));
        assert!(option_takes_value(&emacs, "--execute"));
    }

    #[test]
    fn created_directory_event_wins_when_open_and_create_are_coalesced() {
        let path = PathBuf::from("new.txt");
        let events = coalesce_directory_events(vec![
            DirectoryEvent {
                path: path.clone(),
                created: false,
            },
            DirectoryEvent {
                path: path.clone(),
                created: true,
            },
        ]);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].path, path);
        assert!(events[0].created);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn directory_snapshot_distinguishes_existing_and_new_discovery_targets() {
        let base = env::temp_dir().join(format!("bedit-directory-snapshot-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        let existing = base.join("existing.txt");
        let created = base.join("created.txt");
        fs::write(&existing, b"before\n").unwrap();
        let track = Track {
            path: existing.clone(),
            leaf: "existing.txt".into(),
            baseline: b"before\n".to_vec(),
            observed: Some(b"before\n".to_vec()),
            published: false,
        };
        let discovery = DirectoryEventDiscovery::new(&[track]).unwrap();
        fs::write(&created, b"after\n").unwrap();

        assert_eq!(discovery.knew(&existing), Some(true));
        assert_eq!(discovery.knew(&created), Some(false));
        assert_eq!(
            discovery.knew(Path::new("/usr/lib/libSystem.B.dylib")),
            None
        );
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn exclusion_wildcards_match_legacy_patterns() {
        assert!(wildcard("*.log", "app.log"));
        assert!(!wildcard("*.log", "app.txt"));
    }

    #[test]
    fn dynamic_bvi_candidate_filter_is_conservative() {
        assert!(!discoverable_fd_name("0"));
        assert!(!discoverable_fd_name("1"));
        assert!(!discoverable_fd_name("2"));
        assert!(discoverable_fd_name("3"));

        let base = env::temp_dir().join(format!("bedit-discovery-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let home = base.join("home");
        let work = base.join("work");
        let runtime = base.join("vim-runtime");
        let runtime_roots = [runtime.clone()];
        fs::create_dir_all(home.join(".vim/plugin")).unwrap();
        fs::create_dir_all(home.join(".config/vim")).unwrap();
        fs::create_dir_all(&work).unwrap();
        fs::create_dir_all(&runtime).unwrap();
        fs::create_dir_all(base.join("store/access")).unwrap();
        let store = discovery_store(&base);

        let ordinary = work.join("b.txt");
        fs::write(&ordinary, b"ordinary\n").unwrap();
        let metadata = fs::metadata(&ordinary).unwrap();
        assert_eq!(
            discovery_candidate(&store, &home, &runtime_roots, &[], &ordinary, &metadata),
            Ok(fs::canonicalize(&ordinary).unwrap())
        );

        let tracked = vec![fs::canonicalize(&ordinary).unwrap()];
        assert_eq!(
            discovery_candidate(
                &store,
                &home,
                &runtime_roots,
                &tracked,
                &ordinary,
                &metadata
            ),
            Err(DiscoveryRejection::AlreadyTracked)
        );
        assert_eq!(
            discovery_candidate(
                &store,
                &home,
                &runtime_roots,
                &tracked,
                &ordinary,
                &metadata
            ),
            Err(DiscoveryRejection::AlreadyTracked)
        );

        let internal = base.join("store/access/record");
        fs::write(&internal, b"internal\n").unwrap();
        assert_eq!(
            discovery_candidate(
                &store,
                &home,
                &runtime_roots,
                &[],
                &internal,
                &fs::metadata(&internal).unwrap()
            ),
            Err(DiscoveryRejection::RepositoryInternal)
        );

        for path in [
            Path::new("/proc/self/status"),
            Path::new("/sys/kernel"),
            Path::new("/dev/null"),
        ] {
            if !path.exists() {
                continue;
            }
            assert!(discovery_candidate(
                &store,
                &home,
                &runtime_roots,
                &[],
                path,
                &fs::metadata(path).unwrap()
            )
            .is_err());
        }

        assert_eq!(
            discovery_candidate(
                &store,
                &home,
                &runtime_roots,
                &[],
                Path::new("/tmp/gone (deleted)"),
                &metadata
            ),
            Err(DiscoveryRejection::Deleted)
        );
        assert_eq!(
            discovery_candidate(
                &store,
                &home,
                &runtime_roots,
                &[],
                &work,
                &fs::metadata(&work).unwrap()
            ),
            Err(DiscoveryRejection::NotRegular)
        );

        let socket_path = work.join("editor.sock");
        let _socket = UnixListener::bind(&socket_path).unwrap();
        assert_eq!(
            discovery_candidate(
                &store,
                &home,
                &runtime_roots,
                &[],
                &socket_path,
                &fs::symlink_metadata(&socket_path).unwrap()
            ),
            Err(DiscoveryRejection::NotRegular)
        );

        for path in [
            runtime.join("defaults.vim"),
            home.join(".vim/plugin/example.vim"),
            home.join(".config/vim/vimrc"),
        ] {
            fs::write(&path, b"vim asset\n").unwrap();
            assert_eq!(
                discovery_candidate(
                    &store,
                    &home,
                    &runtime_roots,
                    &[],
                    &path,
                    &fs::metadata(&path).unwrap()
                ),
                Err(DiscoveryRejection::VimAsset)
            );
        }

        for leaf in [
            "vimrc",
            ".vimrc",
            "outside-runtime.vim",
            "syntax.vim",
            "synload.vim",
            "syncolor.vim",
            "filetype.vim",
            "netrwPlugin.vim",
            ".a.txt.swp",
            ".a.txt.swo",
            ".a.txt.swn",
            "a.txt~",
            ".a.txt.tmp",
            "app.log",
        ] {
            let path = work.join(leaf);
            fs::write(&path, b"reject\n").unwrap();
            assert!(
                discovery_candidate(
                    &store,
                    &home,
                    &runtime_roots,
                    &[],
                    &path,
                    &fs::metadata(&path).unwrap()
                )
                .is_err(),
                "accepted {leaf}"
            );
        }

        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn linux_discovery_profiles_cover_supported_wrappers() {
        assert_eq!(discovery_profile("bvi"), Some(DiscoveryProfile::Vim));
        assert_eq!(discovery_profile("bnvim"), Some(DiscoveryProfile::Neovim));
        assert_eq!(discovery_profile("bnano"), Some(DiscoveryProfile::Nano));
        assert_eq!(discovery_profile("bpico"), None);
        assert_eq!(discovery_profile("bemacs"), Some(DiscoveryProfile::Emacs));
        assert_eq!(discovery_profile("bed"), None);
    }

    #[test]
    fn every_spawned_alias_uses_its_family_discovery_profile() {
        for alias in crate::editor::EDITOR_ALIASES.iter().filter(|alias| {
            alias.supported && alias.strategy == crate::editor::ProcessStrategy::SpawnedEditor
        }) {
            let expected = match alias.family {
                EditorFamily::Vim => Some(DiscoveryProfile::Vim),
                EditorFamily::Neovim => Some(DiscoveryProfile::Neovim),
                EditorFamily::Nano => Some(DiscoveryProfile::Nano),
                EditorFamily::Emacs => Some(DiscoveryProfile::Emacs),
                EditorFamily::Ed
                | EditorFamily::Micro
                | EditorFamily::Joe
                | EditorFamily::Jed
                | EditorFamily::McEdit => None,
            };
            assert_eq!(
                family_discovery_profile(alias.family),
                expected,
                "{}",
                alias.name
            );
        }
    }

    #[test]
    fn neovim_assets_are_rejected_without_banning_user_lua() {
        let base = env::temp_dir().join(format!("bedit-neovim-filter-{}", std::process::id()));
        let home = base.join("home");
        let work = base.join("work");
        let config = home.join(".config/nvim");
        let data = home.join(".local/share/nvim/site/pack/plugin");
        fs::create_dir_all(&work).unwrap();
        fs::create_dir_all(&config).unwrap();
        fs::create_dir_all(&data).unwrap();
        fs::create_dir_all(base.join("store/access")).unwrap();
        let store = discovery_store(&base);
        let roots = discovery_roots(DiscoveryProfile::Neovim, &home);

        for user_file in [work.join("plugin.lua"), work.join("project.vim")] {
            fs::write(&user_file, b"user code\n").unwrap();
            assert!(
                profiled_discovery_candidate(
                    &store,
                    DiscoveryProfile::Neovim,
                    &home,
                    &roots,
                    &[],
                    &user_file,
                    &fs::metadata(&user_file).unwrap()
                )
                .is_ok(),
                "rejected project file {}",
                user_file.display()
            );
        }

        for path in [config.join("init.lua"), data.join("plugin.lua")] {
            fs::write(&path, b"runtime\n").unwrap();
            assert_eq!(
                profiled_discovery_candidate(
                    &store,
                    DiscoveryProfile::Neovim,
                    &home,
                    &roots,
                    &[],
                    &path,
                    &fs::metadata(&path).unwrap()
                ),
                Err(DiscoveryRejection::VimAsset)
            );
        }
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn emacs_assets_and_temporary_files_are_rejected_without_banning_user_elisp() {
        let base = env::temp_dir().join(format!("bedit-emacs-filter-{}", std::process::id()));
        let home = base.join("home");
        let work = base.join("work");
        fs::create_dir_all(home.join(".emacs.d/elpa/package")).unwrap();
        fs::create_dir_all(&work).unwrap();
        fs::create_dir_all(base.join("store/access")).unwrap();
        let store = discovery_store(&base);
        let user_elisp = work.join("program.el");
        fs::write(&user_elisp, b"user elisp\n").unwrap();
        assert!(profiled_discovery_candidate(
            &store,
            DiscoveryProfile::Emacs,
            &home,
            &[],
            &[],
            &user_elisp,
            &fs::metadata(&user_elisp).unwrap()
        )
        .is_ok());
        for path in [
            home.join(".emacs.d/init.el"),
            home.join(".emacs.d/elpa/package/autoloads.el"),
            work.join(".#locked.txt"),
            work.join("#autosave.txt#"),
            work.join("saved.txt~"),
        ] {
            fs::write(&path, b"reject\n").unwrap();
            assert!(profiled_discovery_candidate(
                &store,
                DiscoveryProfile::Emacs,
                &home,
                &[],
                &[],
                &path,
                &fs::metadata(&path).unwrap()
            )
            .is_err());
        }
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn parses_machine_lsof_records_conservatively() {
        let output = b"p42\nf0\nn/dev/null\nf3\nn/Users/alice/My Project/a.txt\nf4\nn/Users/alice/na\xC3\xAFve.txt\nf4\nn/Users/alice/na\xC3\xAFve.txt\nfcwd\nn/Users/alice/My Project\nf5\nnrelative.txt\nfbroken\nn/tmp/ignored\n";
        assert_eq!(
            parse_lsof_paths(output),
            vec![
                PathBuf::from("/Users/alice/My Project/a.txt"),
                PathBuf::from("/Users/alice/na\u{00ef}ve.txt"),
            ]
        );
    }

    #[test]
    fn lsof_backend_reports_missing_failure_and_timeout() {
        let missing = LsofDiscovery::new(
            PathBuf::from("/definitely/missing/bedit-lsof"),
            Duration::from_millis(50),
        );
        assert_eq!(
            missing.discover_open_files(42).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );

        let failure = env::temp_dir().join(format!("bedit-failing-lsof-{}", std::process::id()));
        fs::write(&failure, b"#!/bin/sh\nexit 1\n").unwrap();
        fs::set_permissions(&failure, fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(
            LsofDiscovery::new(failure.clone(), Duration::from_secs(1))
                .discover_open_files(42)
                .unwrap_err()
                .kind(),
            io::ErrorKind::Other
        );
        fs::remove_file(failure).unwrap();

        use std::os::unix::fs::PermissionsExt;
        let success = env::temp_dir().join(format!("bedit-good-lsof-{}", std::process::id()));
        fs::write(
            &success,
            b"#!/bin/sh\n[ \"$1\" = -Fn ] && [ \"$2\" = -p ] && [ \"$3\" = 42 ] || exit 9\nprintf 'p42\\nf3\\nn/Users/alice/Project/file with spaces.txt\\n'\n",
        )
        .unwrap();
        fs::set_permissions(&success, fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(
            LsofDiscovery::new(success.clone(), Duration::from_secs(1))
                .discover_open_files(42)
                .unwrap(),
            vec![PathBuf::from("/Users/alice/Project/file with spaces.txt")]
        );
        fs::remove_file(success).unwrap();

        let helper = env::temp_dir().join(format!("bedit-slow-lsof-{}", std::process::id()));
        fs::write(&helper, b"#!/bin/sh\nsleep 5\n").unwrap();
        fs::set_permissions(&helper, fs::Permissions::from_mode(0o700)).unwrap();
        let timeout = LsofDiscovery::new(helper.clone(), Duration::from_millis(20));
        let started = Instant::now();
        assert_eq!(
            timeout.discover_open_files(42).unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
        assert!(started.elapsed() < Duration::from_secs(1));
        fs::remove_file(helper).unwrap();
    }

    #[test]
    fn macos_discovery_uses_human_scale_cadence_and_filters_system_assets() {
        assert_eq!(macos_discovery_interval(None), Duration::from_millis(300));
        assert_eq!(
            macos_discovery_interval(Some("0.125")),
            Duration::from_millis(125)
        );

        let base = env::temp_dir().join(format!("bedit-macos-filter-{}", std::process::id()));
        let home = base.join("Users/alice");
        let work = home.join("Project");
        fs::create_dir_all(&work).unwrap();
        fs::create_dir_all(base.join("store/access")).unwrap();
        let store = discovery_store(&base);
        let user = work.join("normal file.el");
        fs::write(&user, b"user\n").unwrap();
        assert!(profiled_discovery_candidate(
            &store,
            DiscoveryProfile::Emacs,
            &home,
            &macos_discovery_roots(DiscoveryProfile::Emacs, &home),
            &[],
            &user,
            &fs::metadata(&user).unwrap()
        )
        .is_ok());
        assert!(macos_system_asset(Path::new(
            "/System/Library/Frameworks/AppKit.framework/AppKit"
        )));
        assert!(!macos_system_asset(Path::new(
            "/Volumes/Project/ordinary.txt"
        )));
        assert!(!macos_system_asset(Path::new(
            "/private/var/folders/project/ordinary.txt"
        )));
        assert!(!macos_system_asset(Path::new(
            "/private/var/folders/ab/cd/T/project/ordinary.txt"
        )));
        assert!(macos_system_asset(Path::new(
            "/private/var/folders/ab/cd/T/emacs-temp"
        )));
        fs::remove_dir_all(base).unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_directory_discovery_reports_new_editor_files_once() {
        let base = env::temp_dir().join(format!(
            "bedit-macos-directory-discovery-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        let initial = base.join("a.txt");
        fs::write(&initial, b"before\n").unwrap();
        let tracks = [Track {
            path: initial,
            leaf: "a.txt".to_owned(),
            observed: Some(b"before\n".to_vec()),
            baseline: b"before\n".to_vec(),
            published: false,
        }];
        let mut discovery = DirectoryEventDiscovery::new(&tracks).unwrap();
        let initial_events = discovery.drain().unwrap();
        assert!(initial_events
            .iter()
            .any(|event| event.path == base.join("a.txt") && !event.created));
        assert!(discovery.drain().unwrap().is_empty());

        let added = base.join("b.txt");
        fs::write(&added, b"new\n").unwrap();
        let events = discovery.drain().unwrap();
        assert!(events
            .iter()
            .any(|event| event.path == added && event.created));
        assert!(!discovery
            .drain()
            .unwrap()
            .iter()
            .any(|event| event.path == added));
        fs::remove_dir_all(base).unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_platform_selects_lsof_discovery_for_supported_profiles() {
        assert!(platform_discovery(DiscoveryProfile::Vim).is_some());
        assert!(platform_discovery(DiscoveryProfile::Neovim).is_some());
        assert!(platform_discovery(DiscoveryProfile::Nano).is_some());
        assert!(platform_discovery(DiscoveryProfile::Emacs).is_some());
    }

    #[test]
    fn session_saves_and_dynamically_added_tracks_share_dual_targets() {
        let base = env::temp_dir().join(format!("bedit-dual-session-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("work")).unwrap();
        let base = fs::canonicalize(base).unwrap();
        let first = base.join("work/a.txt");
        let second = base.join("work/b.txt");
        fs::write(&first, "one\n").unwrap();
        fs::write(&second, "alpha\n").unwrap();
        let mut root = Config::load(base.join("root-store"), &base.join("missing"), &base).unwrap();
        root.history_owner = "root".into();
        root.actor = "faf".into();
        let mut user = Config::load(base.join("faf-store"), &base.join("missing"), &base).unwrap();
        user.history_owner = "faf".into();
        user.actor = "faf".into();
        let store = Store::with_mirror(root, user);
        let mut first_track = Track {
            path: first.clone(),
            leaf: "a.txt".into(),
            baseline: b"one\n".to_vec(),
            observed: Some(b"one\n".to_vec()),
            published: false,
        };
        let mut second_track = Track {
            path: second.clone(),
            leaf: "b.txt".into(),
            baseline: b"alpha\n".to_vec(),
            observed: Some(b"alpha\n".to_vec()),
            published: false,
        };
        let mut results = SessionResults::default();
        fs::write(&first, "two\n").unwrap();
        poll_track(
            &store,
            &mut first_track,
            "vi",
            true,
            Duration::ZERO,
            &mut results,
        )
        .unwrap();
        fs::write(&first, "three\n").unwrap();
        poll_track(
            &store,
            &mut first_track,
            "vi",
            true,
            Duration::ZERO,
            &mut results,
        )
        .unwrap();
        fs::write(&second, "beta\n").unwrap();
        poll_track(
            &store,
            &mut second_track,
            "vi",
            true,
            Duration::ZERO,
            &mut results,
        )
        .unwrap();
        let root_revisions = store.revisions().unwrap();
        let mirror_revisions = Store::new(store.mirror_config().unwrap().clone())
            .revisions()
            .unwrap();
        assert_eq!(root_revisions.len(), 3);
        assert_eq!(mirror_revisions.len(), 3);
        assert_eq!(
            root_revisions.iter().filter(|r| r.leaf == "a.txt").count(),
            2
        );
        assert_eq!(
            mirror_revisions
                .iter()
                .filter(|r| r.leaf == "a.txt")
                .count(),
            2
        );
        assert!(root_revisions.iter().all(|r| r.actor == "faf"));
        assert!(mirror_revisions.iter().all(|r| r.actor == "faf"));
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn changed_save_states_publish_once_and_final_flush_is_idempotent() {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let base = env::temp_dir().join(format!(
            "bedit-save-semantics-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("work")).unwrap();
        let base = fs::canonicalize(base).unwrap();
        let path = base.join("work/demo.txt");
        fs::write(&path, "original\n").unwrap();
        let store = discovery_store(&base);
        let mut track = Track {
            path: path.clone(),
            leaf: "demo.txt".into(),
            baseline: b"original\n".to_vec(),
            observed: Some(b"original\n".to_vec()),
            published: false,
        };
        let mut results = SessionResults::default();

        for (expected, content) in [(1, "one\n"), (2, "two\n"), (3, "three\n")] {
            fs::write(&path, content).unwrap();
            poll_track(
                &store,
                &mut track,
                "vim",
                true,
                Duration::ZERO,
                &mut results,
            )
            .unwrap();
            assert_eq!(store.revisions().unwrap().len(), expected);
        }

        // A no-op save and the wrapper's exit flush observe the same state and
        // therefore must not publish duplicates.
        poll_track(
            &store,
            &mut track,
            "vim",
            true,
            Duration::ZERO,
            &mut results,
        )
        .unwrap();
        poll_track(
            &store,
            &mut track,
            "vim",
            true,
            Duration::ZERO,
            &mut results,
        )
        .unwrap();
        assert_eq!(store.revisions().unwrap().len(), 3);
        assert_eq!(results.files[&path].revisions, 3);
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn late_create_event_preserves_dynamic_first_save() {
        let base = env::temp_dir().join(format!(
            "bedit-late-create-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("work")).unwrap();
        let base = fs::canonicalize(base).unwrap();
        let path = base.join("work/b.txt");
        fs::write(&path, "b saved\n").unwrap();
        let store = discovery_store(&base);
        let mut tracks = Vec::new();

        // Process inspection can observe Vim's open file after the first write,
        // before the directory watcher reports that the path was newly created.
        assert!(add_discovered_target(
            &store,
            DiscoveryProfile::Vim,
            &base,
            &[],
            &mut tracks,
            &path,
            false,
        )
        .is_some());
        assert_eq!(tracks[0].baseline, b"b saved\n");

        // The authoritative create event must promote the existing track rather
        // than letting the completed first save remain its own baseline.
        let _ = add_discovered_target(
            &store,
            DiscoveryProfile::Vim,
            &base,
            &[],
            &mut tracks,
            &path,
            true,
        );
        let mut results = SessionResults::default();
        poll_track(
            &store,
            &mut tracks[0],
            "vim",
            true,
            Duration::ZERO,
            &mut results,
        )
        .unwrap();

        let revisions = store.revisions().unwrap();
        assert_eq!(revisions.len(), 1);
        assert_eq!(revisions[0].leaf, "b.txt");
        assert_eq!(store.render(&revisions[0]).unwrap(), b"b saved\n");
        assert!(store.backup(&revisions[0]).unwrap().is_empty());

        // A wrapper exit flush after the publication remains idempotent.
        poll_track(
            &store,
            &mut tracks[0],
            "vim",
            true,
            Duration::ZERO,
            &mut results,
        )
        .unwrap();
        assert_eq!(store.revisions().unwrap().len(), 1);
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn failed_publication_does_not_consume_completed_state() {
        let base = env::temp_dir().join(format!(
            "bedit-wrapper-publish-failure-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("work")).unwrap();
        let base = fs::canonicalize(base).unwrap();
        let path = base.join("work/demo.txt");
        fs::write(&path, "before\n").unwrap();
        let store = discovery_store(&base);
        fs::write(&store.config().root, "not a directory").unwrap();
        let mut track = Track {
            path: path.clone(),
            leaf: "demo.txt".into(),
            baseline: b"before\n".to_vec(),
            observed: Some(b"before\n".to_vec()),
            published: false,
        };
        let mut results = SessionResults::default();
        fs::write(&path, "after\n").unwrap();

        assert!(poll_track(
            &store,
            &mut track,
            "vim",
            true,
            Duration::ZERO,
            &mut results,
        )
        .is_err());
        assert_eq!(track.baseline, b"before\n");
        assert_eq!(track.observed.as_deref(), Some(b"before\n".as_slice()));

        fs::remove_file(&store.config().root).unwrap();
        poll_track(
            &store,
            &mut track,
            "vim",
            true,
            Duration::ZERO,
            &mut results,
        )
        .unwrap();
        assert_eq!(store.revisions().unwrap().len(), 1);
        assert_eq!(
            store.render(&store.revisions().unwrap()[0]).unwrap(),
            b"after\n"
        );
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn truncate_then_write_is_deferred_without_losing_completed_state() {
        let base = env::temp_dir().join(format!(
            "bedit-unstable-save-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("work")).unwrap();
        let base = fs::canonicalize(base).unwrap();
        let path = base.join("work/demo.txt");
        fs::write(&path, "before\n").unwrap();
        let store = discovery_store(&base);
        let mut track = Track {
            path: path.clone(),
            leaf: "demo.txt".into(),
            baseline: b"before\n".to_vec(),
            observed: Some(b"before\n".to_vec()),
            published: false,
        };
        let mut results = SessionResults::default();

        fs::write(&path, b"").unwrap();
        let writer_path = path.clone();
        let first_read = std::sync::Arc::new(std::sync::Barrier::new(2));
        let writer_first_read = first_read.clone();
        let writer = thread::spawn(move || {
            writer_first_read.wait();
            fs::write(writer_path, "after\n").unwrap();
        });
        poll_track_after_first_read(
            &store,
            &mut track,
            "vim",
            false,
            Duration::from_millis(50),
            &mut results,
            || {
                first_read.wait();
            },
        )
        .unwrap();
        writer.join().unwrap();
        assert!(store.revisions().unwrap().is_empty());

        poll_track(
            &store,
            &mut track,
            "vim",
            false,
            Duration::ZERO,
            &mut results,
        )
        .unwrap();
        assert_eq!(store.revisions().unwrap().len(), 1);
        assert_eq!(
            store.render(&store.revisions().unwrap()[0]).unwrap(),
            b"after\n"
        );
        fs::remove_dir_all(base).unwrap();
    }
}
