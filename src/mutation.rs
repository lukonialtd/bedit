use crate::paths::{directory_key, record_name};
use crate::store::{Revision, Store};
use crate::trusted_fs::TrustedDir;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static NONCE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub struct CreatedRevision {
    pub number: u64,
    pub backup_only_fallback: bool,
    pub mirror_warning: Option<String>,
}

struct RevisionPayload<'a> {
    backup: &'a [u8],
    diff: Option<&'a [u8]>,
}

struct PreparedRevision<'a> {
    full: &'a Path,
    key: &'a str,
    leaf: &'a str,
    editor: &'a str,
    epoch: u64,
    payload: RevisionPayload<'a>,
}

pub fn write_tag(store: &Store, revision: &Revision, text: &str) -> io::Result<()> {
    let repository = Repository::open(store.config())?;
    let _lock = repository.lock()?;
    let name = record_name(&revision.key, &revision.leaf, revision.number, 't');
    repository.tags.write_replace(
        &name,
        &staging_name("tag"),
        format!("{text}\n").as_bytes(),
        0o600,
    )
}

pub fn create_revision(
    store: &Store,
    path: &Path,
    editor: &str,
    baseline: &[u8],
    represented: &[u8],
) -> io::Result<CreatedRevision> {
    let directory = path.parent().unwrap_or(Path::new("/"));
    let leaf = path
        .file_name()
        .and_then(|v| v.to_str())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid file name"))?;
    let key = directory_key(directory);
    let generated = generate_diff(path, baseline, represented);
    let (backup, diff, fallback) = match generated {
        Ok(diff) if !diff.is_empty() => (baseline, Some(diff), false),
        Ok(_) => (represented, None, false),
        Err(_) => (represented, None, true),
    };
    let epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let prepared = PreparedRevision {
        full: path,
        key: &key,
        leaf,
        editor,
        epoch,
        payload: RevisionPayload {
            backup,
            diff: diff.as_deref(),
        },
    };
    let number = allocate_and_publish(store.config(), &prepared)?;
    let mirror_warning = store.mirror_config().and_then(|mirror| {
        publish_mirror(mirror, &prepared)
            .err()
            .map(|error| format!("user history mirror failed: {error}"))
    });
    Ok(CreatedRevision {
        number,
        backup_only_fallback: fallback,
        mirror_warning,
    })
}

fn allocate_and_publish(
    config: &crate::config::Config,
    prepared: &PreparedRevision<'_>,
) -> io::Result<u64> {
    let repository = Repository::open(config)?;
    let _lock = repository.lock()?;
    let number = repository.next_revision(prepared.key, prepared.leaf)?;
    repository.publish(number, prepared)?;
    Ok(number)
}

fn publish_mirror(
    config: &crate::config::Config,
    prepared: &PreparedRevision<'_>,
) -> io::Result<()> {
    let Some((uid, gid)) = config.ownership else {
        return allocate_and_publish(config, prepared).map(|_| ());
    };
    config.validate_repository_topology()?;
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (uid, gid, prepared);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "secure privileged user-history mirroring is currently supported only on Linux; use root_only",
        ))
    }
    #[cfg(target_os = "linux")]
    unsafe {
        let pid = libc::fork();
        if pid < 0 {
            return Err(io::Error::last_os_error());
        }
        if pid == 0 {
            let ok = libc::setgroups(0, std::ptr::null()) == 0
                && libc::setgid(gid) == 0
                && libc::setuid(uid) == 0
                && libc::geteuid() == uid
                && libc::getegid() == gid;
            let mut unprivileged = config.clone();
            unprivileged.ownership = None;
            let code = if ok && allocate_and_publish(&unprivileged, prepared).is_ok() {
                0
            } else {
                1
            };
            libc::_exit(code);
        }
        let mut status = 0;
        loop {
            if libc::waitpid(pid, &mut status, 0) >= 0 {
                break;
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
        if libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0 {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "secure unprivileged mirror publisher rejected the repository",
            ))
        }
    }
}

struct RepositoryLock(File);

impl RepositoryLock {
    fn acquire(root: &TrustedDir) -> io::Result<Self> {
        let file = root.open_or_create_regular(".bedit-publication.lock", 0o600)?;
        loop {
            if unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(&file), libc::LOCK_EX) } == 0 {
                return Ok(Self(file));
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }
}

impl Drop for RepositoryLock {
    fn drop(&mut self) {
        unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(&self.0), libc::LOCK_UN) };
    }
}

pub fn replace_live(path: &Path, data: &[u8]) -> io::Result<()> {
    let parent_path = path.parent().unwrap_or(Path::new("/"));
    let leaf = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid live file name"))?;
    let parent = TrustedDir::open_absolute(parent_path)?;
    let permissions = parent.open_regular(leaf, false)?.metadata()?.permissions();
    parent.write_replace(
        leaf,
        &staging_name("restore"),
        data,
        permissions.mode() & 0o7777,
    )
}

pub fn restore_revision(
    store: &Store,
    revision: &Revision,
    rendered: bool,
    editor: &str,
) -> io::Result<CreatedRevision> {
    let path = &revision.access.path;
    let live = fs::read(path)?;
    let mut same_file: Vec<_> = store
        .revisions()?
        .into_iter()
        .filter(|r| r.access.path == *path)
        .collect();
    same_file.sort_by_key(|r| r.number);
    let latest = same_file
        .last()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "revision not found"))?;
    let represented = store.render(latest)?;
    let sync_warning = if represented != live {
        create_revision(store, path, "sync", &represented, &live)?.mirror_warning
    } else {
        None
    };
    let target = if rendered {
        store.render(revision)?
    } else {
        fs::read(&revision.backup)?
    };
    replace_live(path, &target)?;
    match create_revision(store, path, editor, &live, &target) {
        Ok(mut created) => {
            created.mirror_warning = match (sync_warning, created.mirror_warning) {
                (Some(first), Some(second)) => Some(format!("{first}; {second}")),
                (Some(warning), None) | (None, Some(warning)) => Some(warning),
                (None, None) => None,
            };
            Ok(created)
        }
        Err(error) => {
            let _ = replace_live(path, &live);
            Err(error)
        }
    }
}

struct Repository {
    root: TrustedDir,
    access: TrustedDir,
    backups: TrustedDir,
    edits: TrustedDir,
    dirs: TrustedDir,
    tags: TrustedDir,
    actors: TrustedDir,
    paths: RepositoryPaths,
    actor: String,
    history_owner: String,
}

struct RepositoryPaths {
    access: PathBuf,
    backups: PathBuf,
    edits: PathBuf,
}

impl Repository {
    fn open(config: &crate::config::Config) -> io::Result<Self> {
        let root = TrustedDir::open_or_create_absolute(&config.root, 0o700)?;
        let access = child_for_config(&root, &config.root, &config.access)?;
        let backups = child_for_config(&root, &config.root, &config.backups)?;
        let edits = child_for_config(&root, &config.root, &config.edits)?;
        let dirs = child_for_config(&root, &config.root, &config.dirs)?;
        let tags = child_for_config(&root, &config.root, &config.tags)?;
        let actors = child_for_config(&root, &config.root, &config.actors)?;
        let repository = Self {
            root,
            access,
            backups,
            edits,
            dirs,
            tags,
            actors,
            paths: RepositoryPaths {
                access: config.access.clone(),
                backups: config.backups.clone(),
                edits: config.edits.clone(),
            },
            actor: config.actor.clone(),
            history_owner: config.history_owner.clone(),
        };
        repository_open_checkpoint(&config.root)?;
        Ok(repository)
    }

    fn lock(&self) -> io::Result<RepositoryLock> {
        RepositoryLock::acquire(&self.root)
    }

    fn next_revision(&self, key: &str, leaf: &str) -> io::Result<u64> {
        let mut maximum = 0;
        for name in self.access.entries()? {
            let Some(name) = name.to_str() else { continue };
            let Some((candidate_key, candidate_leaf, number)) =
                crate::paths::parse_record_name(name, 'a')
            else {
                continue;
            };
            // Enumeration validates every recognized allocation record while
            // the lock is held, even when it belongs to another file.
            let bytes = self.access.read_file(name)?;
            let _ = crate::store::parse_access_bytes(&bytes)?;
            if candidate_key == key && candidate_leaf == leaf {
                maximum = maximum.max(number);
            }
        }
        maximum
            .checked_add(1)
            .ok_or_else(|| io::Error::other("revision number overflow"))
    }

    fn publish(&self, number: u64, prepared: &PreparedRevision<'_>) -> io::Result<()> {
        let PreparedRevision {
            full,
            key,
            leaf,
            editor,
            epoch,
            payload,
        } = prepared;
        let branch = self.dirs.child_dir_create(key, 0o700)?;
        branch.write_replace(
            ".branch",
            &staging_name("branch"),
            format!("{}\n", full.parent().unwrap().display()).as_bytes(),
            0o600,
        )?;

        let backup_name = record_name(key, leaf, number, 'b');
        let diff_name = record_name(key, leaf, number, 'd');
        let access_name = record_name(key, leaf, number, 'a');
        let actor_name = record_name(key, leaf, number, 'u');
        let mut created: Vec<(&TrustedDir, String)> = Vec::new();
        let result = (|| {
            self.backups.write_noreplace(
                &backup_name,
                &staging_name("backup"),
                payload.backup,
                0o600,
            )?;
            created.push((&self.backups, backup_name.clone()));
            if let Some(bytes) = payload.diff {
                self.edits
                    .write_noreplace(&diff_name, &staging_name("diff"), bytes, 0o600)?;
                created.push((&self.edits, diff_name.clone()));
            }
            let stamp = timestamp(*epoch);
            self.access.write_noreplace(
                &access_name,
                &staging_name("access"),
                format!("{epoch}\t{stamp}\t{}\t{editor}\n", full.display()).as_bytes(),
                0o600,
            )?;
            created.push((&self.access, access_name.clone()));
            if self.actor != self.history_owner {
                self.actors.write_noreplace(
                    &actor_name,
                    &staging_name("actor"),
                    format!("{}\n", self.actor).as_bytes(),
                    0o600,
                )?;
                created.push((&self.actors, actor_name));
            }
            publish_index_link(
                &branch,
                leaf,
                number,
                'b',
                &self.paths.backups.join(&backup_name),
            )?;
            if payload.diff.is_some() {
                publish_index_link(
                    &branch,
                    leaf,
                    number,
                    'd',
                    &self.paths.edits.join(&diff_name),
                )?;
            }
            publish_index_link(
                &branch,
                leaf,
                number,
                'a',
                &self.paths.access.join(&access_name),
            )?;
            Ok(())
        })();
        if result.is_err() {
            for (directory, name) in created.into_iter().rev() {
                let _ = directory.unlink_file(&name);
            }
            for kind in ['a', 'b', 'd'] {
                let _ = branch.unlink_file(&format!("{leaf}::_::{number}::_::{kind}"));
            }
        }
        result
    }
}

fn child_for_config(root: &TrustedDir, root_path: &Path, child: &Path) -> io::Result<TrustedDir> {
    let relative = child.strip_prefix(root_path).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("repository child escapes trusted root: {}", child.display()),
        )
    })?;
    let mut components = relative.components();
    let name = components
        .next()
        .and_then(|value| value.as_os_str().to_str())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid repository child"))?;
    if components.next().is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "repository record directories must be direct children of the trusted root",
        ));
    }
    root.child_dir_create(name, 0o700)
}

fn publish_index_link(
    branch: &TrustedDir,
    leaf: &str,
    number: u64,
    kind: char,
    target: &Path,
) -> io::Result<()> {
    branch.symlink_noreplace(&format!("{leaf}::_::{number}::_::{kind}"), target)
}

fn generate_diff(path: &Path, baseline: &[u8], represented: &[u8]) -> io::Result<Vec<u8>> {
    let scratch = std::env::temp_dir();
    let left = scratch.join(format!(
        ".rust-write-left-{}-{}",
        std::process::id(),
        nonce()
    ));
    let right = scratch.join(format!(
        ".rust-write-right-{}-{}",
        std::process::id(),
        nonce()
    ));
    let write_scratch = |path: &Path, bytes: &[u8]| -> io::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(path)?;
        file.write_all(bytes)
    };
    write_scratch(&left, baseline)?;
    if let Err(error) = write_scratch(&right, represented) {
        let _ = fs::remove_file(&left);
        return Err(error);
    }
    let output = Command::new("diff")
        .args(["-u", "--"])
        .arg(&left)
        .arg(&right)
        .output();
    let _ = fs::remove_file(&left);
    let _ = fs::remove_file(&right);
    let output = output?;
    match output.status.code() {
        Some(0 | 1) => {
            let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
            if let Some(end) = text.find('\n') {
                text.replace_range(..end, &format!("--- {}@before", path.display()));
                if let Some(start) = text.find("\n+++ ").map(|value| value + 1) {
                    if let Some(finish) = text[start..].find('\n').map(|v| v + start) {
                        text.replace_range(start..finish, &format!("+++ {}@saved", path.display()));
                    }
                }
            }
            Ok(text.into_bytes())
        }
        _ => Err(io::Error::other("diff generation failed")),
    }
}

fn staging_name(kind: &str) -> String {
    #[cfg(debug_assertions)]
    if std::env::var_os("BEDIT_TESTING").as_deref() == Some(std::ffi::OsStr::new("1"))
        && std::env::var("BEDIT_TEST_STAGING_KIND").as_deref() == Ok(kind)
    {
        if let Ok(name) = std::env::var("BEDIT_TEST_STAGING_NAME") {
            if !name.is_empty() && !matches!(name.as_str(), "." | "..") && !name.contains('/') {
                return name;
            }
        }
    }
    format!(".bedit-{kind}-{}-{}", std::process::id(), nonce())
}

fn repository_open_checkpoint(_root: &Path) -> io::Result<()> {
    #[cfg(debug_assertions)]
    if std::env::var_os("BEDIT_TESTING").as_deref() == Some(std::ffi::OsStr::new("1")) {
        if std::env::var_os("BEDIT_TEST_REPOSITORY_OPEN_MATCH").as_deref()
            != Some(_root.as_os_str())
        {
            return Ok(());
        }
        if let (Some(marker), Some(release)) = (
            std::env::var_os("BEDIT_TEST_REPOSITORY_OPEN_MARKER"),
            std::env::var_os("BEDIT_TEST_REPOSITORY_OPEN_CONTINUE"),
        ) {
            fs::write(&marker, b"opened\n")?;
            while !Path::new(&release).exists() {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
    }
    Ok(())
}

fn timestamp(epoch: u64) -> String {
    let mut command = Command::new("date");
    command.arg("-u");
    #[cfg(target_os = "linux")]
    command.args(["-d", &format!("@{epoch}")]);
    #[cfg(target_os = "macos")]
    command.args(["-r", &epoch.to_string()]);
    command
        .arg("+%Y-%m-%d %H:%M:%S +0000")
        .output()
        .ok()
        .filter(|v| v.status.success())
        .map(|v| String::from_utf8_lossy(&v.stdout).trim().to_owned())
        .unwrap_or_default()
}

fn nonce() -> u128 {
    let clock = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64 as u128;
    let sequence = NONCE_SEQUENCE.fetch_add(1, Ordering::Relaxed) as u128;
    (clock << 64) | sequence
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::paths::record_path;
    use std::collections::HashSet;
    use std::os::unix::fs::symlink;
    use std::sync::{Arc, Barrier};

    #[test]
    fn generated_diff_rewrites_headers_for_long_destination_paths() {
        let path = PathBuf::from(format!(
            "/opt/homebrew/{}/library.dylib",
            "component".repeat(32)
        ));
        let diff =
            String::from_utf8(generate_diff(&path, b"before\n", b"after\n").unwrap()).unwrap();

        assert!(diff.starts_with(&format!("--- {}@before\n", path.display())));
        assert!(diff.contains(&format!("+++ {}@saved\n", path.display())));
    }

    #[test]
    fn concurrent_scratch_nonces_are_unique() {
        const THREADS: usize = 64;
        const IDS_PER_THREAD: usize = 4_096;
        let barrier = Arc::new(Barrier::new(THREADS));
        let workers = (0..THREADS)
            .map(|_| {
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    (0..IDS_PER_THREAD).map(|_| nonce()).collect::<Vec<_>>()
                })
            })
            .collect::<Vec<_>>();
        let ids = workers
            .into_iter()
            .flat_map(|worker| worker.join().unwrap())
            .collect::<Vec<_>>();
        let unique = ids.iter().copied().collect::<HashSet<_>>();
        assert_eq!(
            unique.len(),
            ids.len(),
            "concurrent scratch nonce collision"
        );
    }

    #[test]
    fn root_policy_history_can_distinguish_two_sudo_actors() {
        let base = std::env::temp_dir().join(format!("bedit-actors-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("work")).unwrap();
        let path = base.join("work/privileged.conf");
        let mut config = Config::load(base.join("store"), &base.join("missing"), &base).unwrap();
        fs::create_dir_all(&config.root).unwrap();
        config.history_owner = "root".into();
        config.actor = "luke".into();
        let first = Store::new(config.clone());
        create_revision(&first, &path, "vi", b"one\n", b"two\n").unwrap();
        config.actor = "pat".into();
        let second = Store::new(config);
        create_revision(&second, &path, "vi", b"two\n", b"three\n").unwrap();
        let revisions = second.revisions().unwrap();
        assert_eq!(
            revisions
                .iter()
                .map(|r| r.actor.as_str())
                .collect::<Vec<_>>(),
            ["luke", "pat"]
        );
        for revision in &revisions {
            let access = record_path(
                &second.config().access,
                &revision.key,
                &revision.leaf,
                revision.number,
                'a',
            );
            assert_eq!(
                fs::read_to_string(access)
                    .unwrap()
                    .trim_end()
                    .split('\t')
                    .count(),
                4
            );
        }
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn canonical_revision_is_preserved_and_equivalently_mirrored() {
        let base = std::env::temp_dir().join(format!("bedit-dual-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("work")).unwrap();
        let path = base.join("work/privileged.conf");
        let mut root = Config::load(base.join("root-store"), &base.join("missing"), &base).unwrap();
        root.history_owner = "root".into();
        root.actor = "faf".into();
        fs::create_dir_all(&root.root).unwrap();
        let mut user = Config::load(base.join("faf-store"), &base.join("missing"), &base).unwrap();
        user.history_owner = "faf".into();
        user.actor = "faf".into();
        fs::create_dir_all(&user.root).unwrap();
        create_revision(
            &Store::new(user.clone()),
            &path,
            "vi",
            b"older\n",
            b"before\n",
        )
        .unwrap();
        let store = Store::with_mirror(root, user);

        let made = create_revision(&store, &path, "vi", b"before\n", b"after\n").unwrap();
        assert!(made.mirror_warning.is_none());
        let root_revision = store.revisions().unwrap().pop().unwrap();
        let user_revision = Store::new(store.mirror_config().unwrap().clone())
            .revisions()
            .unwrap()
            .pop()
            .unwrap();
        assert_eq!(root_revision.actor, "faf");
        assert_eq!(user_revision.actor, "faf");
        assert_eq!(root_revision.number, 1);
        assert_eq!(user_revision.number, 2);
        assert_eq!(root_revision.access.epoch, user_revision.access.epoch);
        assert_eq!(store.render(&root_revision).unwrap(), b"after\n");
        assert_eq!(
            Store::new(store.mirror_config().unwrap().clone())
                .render(&user_revision)
                .unwrap(),
            b"after\n"
        );
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn mirror_failure_keeps_canonical_revision_and_reports_partial_success() {
        let base = std::env::temp_dir().join(format!("bedit-mirror-fail-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("work")).unwrap();
        let path = base.join("work/privileged.conf");
        let mut root = Config::load(base.join("root-store"), &base.join("missing"), &base).unwrap();
        root.history_owner = "root".into();
        root.actor = "faf".into();
        let mut user = Config::load(base.join("blocked"), &base.join("missing"), &base).unwrap();
        user.history_owner = "faf".into();
        user.actor = "faf".into();
        fs::write(&user.root, "not a directory").unwrap();
        let store = Store::with_mirror(root, user);

        let made = create_revision(&store, &path, "vi", b"before\n", b"after\n").unwrap();
        assert!(made.mirror_warning.unwrap().contains("mirror failed"));
        assert_eq!(store.revisions().unwrap().len(), 1);
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn canonical_failure_never_publishes_user_only_revision() {
        let base = std::env::temp_dir().join(format!("bedit-root-fail-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("work")).unwrap();
        let path = base.join("work/privileged.conf");
        let root = Config::load(base.join("blocked"), &base.join("missing"), &base).unwrap();
        fs::write(&root.root, "not a directory").unwrap();
        let user = Config::load(base.join("user-store"), &base.join("missing"), &base).unwrap();
        let user_store = Store::new(user.clone());
        let store = Store::with_mirror(root, user);

        assert!(create_revision(&store, &path, "vi", b"before\n", b"after\n").is_err());
        assert!(user_store.revisions().unwrap().is_empty());
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn two_sudo_users_share_root_history_without_personal_leakage() {
        let base = std::env::temp_dir().join(format!("bedit-two-users-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("work")).unwrap();
        let path = base.join("work/privileged.conf");
        let root_path = base.join("root-store");
        for (actor, personal, before, after) in [
            (
                "bedit",
                "bedit-store",
                b"one\n".as_slice(),
                b"two\n".as_slice(),
            ),
            (
                "faf",
                "faf-store",
                b"two\n".as_slice(),
                b"three\n".as_slice(),
            ),
        ] {
            let mut root = Config::load(root_path.clone(), &base.join("missing"), &base).unwrap();
            root.history_owner = "root".into();
            root.actor = actor.into();
            let mut user = Config::load(base.join(personal), &base.join("missing"), &base).unwrap();
            user.history_owner = actor.into();
            user.actor = actor.into();
            create_revision(&Store::with_mirror(root, user), &path, "vi", before, after).unwrap();
        }
        let root = Store::new(Config::load(root_path, &base.join("missing"), &base).unwrap());
        let mut bedit_config =
            Config::load(base.join("bedit-store"), &base.join("missing"), &base).unwrap();
        bedit_config.history_owner = "bedit".into();
        let bedit = Store::new(bedit_config);
        let mut faf_config =
            Config::load(base.join("faf-store"), &base.join("missing"), &base).unwrap();
        faf_config.history_owner = "faf".into();
        let faf = Store::new(faf_config);
        assert_eq!(
            root.revisions()
                .unwrap()
                .iter()
                .map(|r| r.actor.as_str())
                .collect::<Vec<_>>(),
            ["bedit", "faf"]
        );
        assert_eq!(
            bedit
                .revisions()
                .unwrap()
                .iter()
                .map(|r| r.actor.as_str())
                .collect::<Vec<_>>(),
            ["bedit"]
        );
        assert_eq!(
            faf.revisions()
                .unwrap()
                .iter()
                .map(|r| r.actor.as_str())
                .collect::<Vec<_>>(),
            ["faf"]
        );
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn sudo_restore_mutates_live_once_and_publishes_to_both_targets() {
        let base = std::env::temp_dir().join(format!("bedit-dual-restore-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("work")).unwrap();
        let path = base.join("work/privileged.conf");
        fs::write(&path, "after\n").unwrap();
        let mut root = Config::load(base.join("root-store"), &base.join("missing"), &base).unwrap();
        root.history_owner = "root".into();
        root.actor = "faf".into();
        fs::create_dir_all(&root.root).unwrap();
        let mut user = Config::load(base.join("faf-store"), &base.join("missing"), &base).unwrap();
        user.history_owner = "faf".into();
        user.actor = "faf".into();
        let store = Store::with_mirror(root, user);
        create_revision(&store, &path, "vi", b"before\n", b"after\n").unwrap();
        let revision = store.revisions().unwrap().pop().unwrap();
        restore_revision(&store, &revision, false, "restore-backup").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"before\n");
        assert_eq!(store.revisions().unwrap().len(), 2);
        assert_eq!(
            Store::new(store.mirror_config().unwrap().clone())
                .revisions()
                .unwrap()
                .len(),
            2
        );
        fs::remove_dir_all(base).unwrap();
    }

    fn assert_concurrent_publications(writer_count: usize) {
        let base = fs::canonicalize(std::env::temp_dir())
            .unwrap()
            .join(format!(
                "bedit-concurrent-{}-{}",
                std::process::id(),
                nonce()
            ));
        fs::create_dir_all(base.join("work")).unwrap();
        let path = base.join("work/shared.txt");
        let config = Config::load(base.join("store"), &base.join("missing"), &base).unwrap();
        let mut writers = Vec::new();
        for value in 0..writer_count {
            let path = path.clone();
            let config = config.clone();
            writers.push(std::thread::spawn(move || {
                let before = format!("before-{value}\n");
                let content = format!("writer-{value}\n");
                let result = create_revision(
                    &Store::new(config),
                    &path,
                    "vi",
                    before.as_bytes(),
                    content.as_bytes(),
                );
                (value, result)
            }));
        }
        let mut allocations = Vec::new();
        for writer in writers {
            let (value, result) = writer.join().unwrap();
            let made = result.unwrap_or_else(|error| panic!("writer {value} failed: {error}"));
            assert!(
                !made.backup_only_fallback,
                "writer {value} unexpectedly published a backup-only fallback as revision {}",
                made.number
            );
            allocations.push((value, made.number));
        }
        allocations.sort_by_key(|(_, number)| *number);
        assert_eq!(
            allocations
                .iter()
                .map(|(_, number)| *number)
                .collect::<Vec<_>>(),
            (1..=writer_count as u64).collect::<Vec<_>>()
        );
        let store = Store::new(config);
        let revisions = store.revisions().unwrap();
        assert_eq!(revisions.len(), writer_count);
        assert_eq!(
            revisions
                .iter()
                .map(|revision| revision.number)
                .collect::<Vec<_>>(),
            (1..=writer_count as u64).collect::<Vec<_>>()
        );
        let mut contents = revisions
            .iter()
            .map(|revision| String::from_utf8(store.render(revision).unwrap()).unwrap())
            .collect::<Vec<_>>();
        contents.sort();
        let mut expected = (0..writer_count)
            .map(|value| format!("writer-{value}\n"))
            .collect::<Vec<_>>();
        expected.sort();
        assert_eq!(contents, expected);
        assert_eq!(
            fs::read_dir(&store.config().access).unwrap().count(),
            writer_count
        );
        assert_eq!(
            fs::read_dir(&store.config().backups).unwrap().count(),
            writer_count
        );
        assert_eq!(
            fs::read_dir(&store.config().edits).unwrap().count(),
            writer_count
        );
        assert_eq!(fs::read_dir(&store.config().actors).unwrap().count(), 0);
        for directory in [
            &store.config().root,
            &store.config().access,
            &store.config().backups,
            &store.config().edits,
        ] {
            assert!(fs::read_dir(directory).unwrap().all(|entry| {
                let name = entry.unwrap().file_name();
                !name.to_string_lossy().starts_with(".bedit-") || name == ".bedit-publication.lock"
            }));
        }
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn concurrent_writers_publish_unique_complete_revisions() {
        assert_concurrent_publications(32);
    }

    #[test]
    fn concurrent_publication_is_complete_across_writer_counts() {
        #[cfg(not(target_os = "macos"))]
        for writer_count in [2, 4, 8, 16, 64] {
            assert_concurrent_publications(writer_count);
        }
        #[cfg(target_os = "macos")]
        for writer_count in [2, 4, 8, 16, 32, 32, 64] {
            assert_concurrent_process_publications(writer_count);
        }
    }

    #[cfg(target_os = "macos")]
    fn assert_concurrent_process_publications(writer_count: usize) {
        let base = fs::canonicalize(std::env::temp_dir())
            .unwrap()
            .join(format!(
                "bedit-process-concurrent-{}-{}",
                std::process::id(),
                nonce()
            ));
        fs::create_dir_all(base.join("work")).unwrap();
        let path = base.join("work/shared.txt");
        let store_root = base.join("store");
        let executable = std::env::current_exe().unwrap();
        let workers = (0..writer_count)
            .map(|value| {
                Command::new(&executable)
                    .args([
                        "--exact",
                        "mutation::tests::concurrent_publication_process_worker",
                        "--nocapture",
                    ])
                    .env("BEDIT_PROCESS_WORKER_STORE", &store_root)
                    .env("BEDIT_PROCESS_WORKER_PATH", &path)
                    .env("BEDIT_PROCESS_WORKER_BASE", &base)
                    .env("BEDIT_PROCESS_WORKER_VALUE", value.to_string())
                    .spawn()
                    .unwrap()
            })
            .collect::<Vec<_>>();
        for mut worker in workers {
            assert!(worker.wait().unwrap().success());
        }
        let config = Config::load(store_root, &base.join("missing"), &base).unwrap();
        let store = Store::new(config);
        let revisions = store.revisions().unwrap();
        assert_eq!(revisions.len(), writer_count);
        assert_eq!(
            revisions
                .iter()
                .map(|revision| revision.number)
                .collect::<Vec<_>>(),
            (1..=writer_count as u64).collect::<Vec<_>>()
        );
        let mut contents = revisions
            .iter()
            .map(|revision| String::from_utf8(store.render(revision).unwrap()).unwrap())
            .collect::<Vec<_>>();
        contents.sort();
        let mut expected = (0..writer_count)
            .map(|value| format!("writer-{value}\n"))
            .collect::<Vec<_>>();
        expected.sort();
        assert_eq!(contents, expected);
        for directory in [
            &store.config().root,
            &store.config().access,
            &store.config().backups,
            &store.config().edits,
        ] {
            assert!(fs::read_dir(directory).unwrap().all(|entry| {
                let name = entry.unwrap().file_name();
                !name.to_string_lossy().starts_with(".bedit-") || name == ".bedit-publication.lock"
            }));
        }
        assert_eq!(
            fs::read_dir(&store.config().access).unwrap().count(),
            writer_count
        );
        assert_eq!(
            fs::read_dir(&store.config().backups).unwrap().count(),
            writer_count
        );
        assert_eq!(
            fs::read_dir(&store.config().edits).unwrap().count(),
            writer_count
        );
        fs::remove_dir_all(base).unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn concurrent_publication_process_worker() {
        let Some(store_root) = std::env::var_os("BEDIT_PROCESS_WORKER_STORE") else {
            return;
        };
        let path = PathBuf::from(std::env::var_os("BEDIT_PROCESS_WORKER_PATH").unwrap());
        let base = PathBuf::from(std::env::var_os("BEDIT_PROCESS_WORKER_BASE").unwrap());
        let value: usize = std::env::var("BEDIT_PROCESS_WORKER_VALUE")
            .unwrap()
            .parse()
            .unwrap();
        let config = Config::load(PathBuf::from(store_root), &base.join("missing"), &base).unwrap();
        let before = format!("before-{value}\n");
        let content = format!("writer-{value}\n");
        create_revision(
            &Store::new(config),
            &path,
            "vi",
            before.as_bytes(),
            content.as_bytes(),
        )
        .unwrap();
    }

    #[test]
    fn mirror_root_symlink_fails_without_touching_victim() {
        let base = std::env::temp_dir().join(format!("bedit-mirror-link-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("work")).unwrap();
        fs::create_dir_all(base.join("victim")).unwrap();
        fs::write(base.join("victim/sentinel"), "unchanged\n").unwrap();
        let path = base.join("work/privileged.conf");
        let root = Config::load(base.join("root-store"), &base.join("missing"), &base).unwrap();
        let user = Config::load(base.join("user-store"), &base.join("missing"), &base).unwrap();
        symlink(base.join("victim"), &user.root).unwrap();
        let made = create_revision(
            &Store::with_mirror(root, user),
            &path,
            "vi",
            b"before\n",
            b"after\n",
        )
        .unwrap();
        assert!(made.mirror_warning.is_some());
        assert_eq!(
            fs::read(base.join("victim/sentinel")).unwrap(),
            b"unchanged\n"
        );
        assert_eq!(fs::read_dir(base.join("victim")).unwrap().count(), 1);
        fs::remove_dir_all(base).unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_privileged_mirror_remains_fail_closed() {
        let base = fs::canonicalize(std::env::temp_dir())
            .unwrap()
            .join(format!("bedit-macos-privileged-boundary-{}", nonce()));
        fs::create_dir_all(base.join("work")).unwrap();
        let path = base.join("work/file.txt");
        let canonical = Config::load(base.join("canonical"), &base.join("missing"), &base).unwrap();
        let mut mirror = Config::load(base.join("mirror"), &base.join("missing"), &base).unwrap();
        mirror.ownership = Some((unsafe { libc::getuid() }, unsafe { libc::getgid() }));
        let created = create_revision(
            &Store::with_mirror(canonical, mirror),
            &path,
            "vi",
            b"before\n",
            b"after\n",
        )
        .unwrap();
        assert!(created
            .mirror_warning
            .unwrap()
            .contains("supported only on Linux"));
        fs::remove_dir_all(base).unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_normal_restore_and_get_semantics() {
        let base = fs::canonicalize(std::env::temp_dir())
            .unwrap()
            .join(format!("bedit-macos-restore-{}", nonce()));
        fs::create_dir_all(base.join("work")).unwrap();
        let path = base.join("work/file.txt");
        fs::write(&path, b"after\n").unwrap();
        let config = Config::load(base.join("store"), &base.join("missing"), &base).unwrap();
        let store = Store::new(config);
        create_revision(&store, &path, "vi", b"before\n", b"after\n").unwrap();
        let revision = store.revisions().unwrap().pop().unwrap();
        assert_eq!(store.backup(&revision).unwrap(), b"before\n");
        assert_eq!(store.render(&revision).unwrap(), b"after\n");
        restore_revision(&store, &revision, false, "restore-backup").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"before\n");
        assert_eq!(store.revisions().unwrap().len(), 2);
        fs::remove_dir_all(base).unwrap();
    }
}
