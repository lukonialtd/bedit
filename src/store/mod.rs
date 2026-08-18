use crate::config::Config;
use crate::paths::{parse_record_name, record_path};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use crate::trusted_fs::TrustedDir;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static RENDER_NONCE: AtomicU64 = AtomicU64::new(0);

fn render_nonce() -> String {
    let epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let sequence = RENDER_NONCE.fetch_add(1, Ordering::Relaxed);
    format!("{}-{epoch}-{sequence}", std::process::id())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessRecord {
    pub epoch: u64,
    pub stamp: String,
    pub path: PathBuf,
    pub editor: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Revision {
    pub key: String,
    pub leaf: String,
    pub number: u64,
    pub access: AccessRecord,
    pub backup: PathBuf,
    pub diff: Option<PathBuf>,
    pub tag: Option<String>,
    pub actor: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexEntry {
    pub branch: PathBuf,
    pub leaf: String,
    pub revision: u64,
    pub kind: char,
    pub target: PathBuf,
}

pub struct Store {
    config: Config,
    mirror: Option<Config>,
}

impl Store {
    pub fn new(config: Config) -> Self {
        Self {
            config,
            mirror: None,
        }
    }

    pub fn with_mirror(config: Config, mirror: Config) -> Self {
        Self {
            config,
            mirror: Some(mirror),
        }
    }

    pub fn from_environment() -> io::Result<Self> {
        let (config, mirror) = Config::targets_from_environment()?;
        Ok(Self { config, mirror })
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn mirror_config(&self) -> Option<&Config> {
        self.mirror.as_ref()
    }

    pub fn revisions(&self) -> io::Result<Vec<Revision>> {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            self.trusted_revisions()
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            self.path_revisions()
        }
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    fn path_revisions(&self) -> io::Result<Vec<Revision>> {
        let mut revisions = Vec::new();
        if !self.config.access.is_dir() {
            return Ok(revisions);
        }
        for entry in fs::read_dir(&self.config.access)? {
            let entry = entry?;
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let Some((key, leaf, number)) = parse_record_name(&name, 'a') else {
                continue;
            };
            let access = parse_access_record(&entry.path())?;
            let backup = record_path(&self.config.backups, &key, &leaf, number, 'b');
            if !backup.is_file() {
                continue;
            }
            let diff_path = record_path(&self.config.edits, &key, &leaf, number, 'd');
            let tag_path = record_path(&self.config.tags, &key, &leaf, number, 't');
            let tag = if tag_path.is_file() {
                Some(
                    fs::read_to_string(tag_path)?
                        .trim_end_matches(['\r', '\n'])
                        .to_owned(),
                )
            } else {
                None
            };
            let actor_path = record_path(&self.config.actors, &key, &leaf, number, 'u');
            let actor = if actor_path.is_file() {
                fs::read_to_string(actor_path)?
                    .trim_end_matches(['\r', '\n'])
                    .to_owned()
            } else {
                self.config.history_owner.clone()
            };
            revisions.push(Revision {
                key,
                leaf,
                number,
                access,
                backup,
                diff: diff_path.is_file().then_some(diff_path),
                tag,
                actor,
            });
        }
        revisions.sort_by(|a, b| {
            a.access
                .path
                .cmp(&b.access.path)
                .then(a.number.cmp(&b.number))
        });
        Ok(revisions)
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn trusted_revisions(&self) -> io::Result<Vec<Revision>> {
        let root = match TrustedDir::open_absolute(&self.config.root) {
            Ok(root) => root,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        let access_dir = match trusted_config_child(&root, &self.config.root, &self.config.access) {
            Ok(directory) => directory,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        let backups_dir = trusted_config_child(&root, &self.config.root, &self.config.backups)?;
        let edits_dir = trusted_config_child(&root, &self.config.root, &self.config.edits)?;
        let tags_dir = trusted_config_child(&root, &self.config.root, &self.config.tags)?;
        let actors_dir = match trusted_config_child(&root, &self.config.root, &self.config.actors) {
            Ok(directory) => Some(directory),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        let mut revisions = Vec::new();
        for name in access_dir.entries()? {
            let Some(name) = name.to_str() else { continue };
            let Some((key, leaf, number)) = parse_record_name(name, 'a') else {
                continue;
            };
            let access = parse_access_bytes(&access_dir.read_file(name)?)?;
            let backup_name = crate::paths::record_name(&key, &leaf, number, 'b');
            if backups_dir.open_regular(&backup_name, false).is_err() {
                continue;
            }
            let diff_name = crate::paths::record_name(&key, &leaf, number, 'd');
            let tag_name = crate::paths::record_name(&key, &leaf, number, 't');
            let actor_name = crate::paths::record_name(&key, &leaf, number, 'u');
            let tag = match tags_dir.read_file(&tag_name) {
                Ok(bytes) => Some(
                    String::from_utf8(bytes)
                        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?
                        .trim_end_matches(['\r', '\n'])
                        .to_owned(),
                ),
                Err(error) if error.kind() == io::ErrorKind::NotFound => None,
                Err(error) => return Err(error),
            };
            let actor = match actors_dir.as_ref().map_or_else(
                || Err(io::Error::from(io::ErrorKind::NotFound)),
                |dir| dir.read_file(&actor_name),
            ) {
                Ok(bytes) => String::from_utf8(bytes)
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?
                    .trim_end_matches(['\r', '\n'])
                    .to_owned(),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    self.config.history_owner.clone()
                }
                Err(error) => return Err(error),
            };
            let diff = match edits_dir.open_regular(&diff_name, false) {
                Ok(_) => Some(record_path(&self.config.edits, &key, &leaf, number, 'd')),
                Err(error) if error.kind() == io::ErrorKind::NotFound => None,
                Err(error) => return Err(error),
            };
            revisions.push(Revision {
                key: key.clone(),
                leaf: leaf.clone(),
                number,
                access,
                backup: record_path(&self.config.backups, &key, &leaf, number, 'b'),
                diff,
                tag,
                actor,
            });
        }
        revisions.sort_by(|a, b| {
            a.access
                .path
                .cmp(&b.access.path)
                .then(a.number.cmp(&b.number))
        });
        Ok(revisions)
    }

    pub fn index_entries(&self) -> io::Result<Vec<IndexEntry>> {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            self.trusted_index_entries()
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            self.path_index_entries()
        }
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    fn path_index_entries(&self) -> io::Result<Vec<IndexEntry>> {
        let mut result = Vec::new();
        if !self.config.dirs.is_dir() {
            return Ok(result);
        }
        for directory in fs::read_dir(&self.config.dirs)? {
            let directory = directory?;
            if !directory.file_type()?.is_dir() {
                continue;
            }
            let branch =
                PathBuf::from(fs::read_to_string(directory.path().join(".branch"))?.trim());
            for entry in fs::read_dir(directory.path())? {
                let entry = entry?;
                let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                    continue;
                };
                if name == ".branch" {
                    continue;
                }
                for kind in ['a', 'b', 'd'] {
                    let synthetic = format!("index::_::{name}");
                    if let Some((_, leaf, revision)) = parse_record_name(&synthetic, kind) {
                        result.push(IndexEntry {
                            branch: branch.clone(),
                            leaf,
                            revision,
                            kind,
                            target: fs::read_link(entry.path())?,
                        });
                        break;
                    }
                }
            }
        }
        result.sort_by(|a, b| {
            a.branch
                .cmp(&b.branch)
                .then(a.leaf.cmp(&b.leaf))
                .then(a.revision.cmp(&b.revision))
                .then(a.kind.cmp(&b.kind))
        });
        Ok(result)
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn trusted_index_entries(&self) -> io::Result<Vec<IndexEntry>> {
        let root = match TrustedDir::open_absolute(&self.config.root) {
            Ok(root) => root,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        let dirs = match trusted_config_child(&root, &self.config.root, &self.config.dirs) {
            Ok(directory) => directory,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        let mut result = Vec::new();
        for name in dirs.entries()? {
            let Some(name) = name.to_str() else { continue };
            let directory = dirs.child_dir(name)?;
            let branch = PathBuf::from(
                String::from_utf8(directory.read_file(".branch")?)
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?
                    .trim(),
            );
            for entry in directory.entries()? {
                let Some(entry) = entry.to_str() else {
                    continue;
                };
                if entry == ".branch" {
                    continue;
                }
                for kind in ['a', 'b', 'd'] {
                    let synthetic = format!("index::_::{entry}");
                    if let Some((_, leaf, revision)) = parse_record_name(&synthetic, kind) {
                        result.push(IndexEntry {
                            branch: branch.clone(),
                            leaf,
                            revision,
                            kind,
                            target: directory.read_link(entry)?,
                        });
                        break;
                    }
                }
            }
        }
        result.sort_by(|a, b| {
            a.branch
                .cmp(&b.branch)
                .then(a.leaf.cmp(&b.leaf))
                .then(a.revision.cmp(&b.revision))
                .then(a.kind.cmp(&b.kind))
        });
        Ok(result)
    }

    pub fn render(&self, revision: &Revision) -> io::Result<Vec<u8>> {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            let root = TrustedDir::open_absolute(&self.config.root)?;
            let backups = trusted_config_child(&root, &self.config.root, &self.config.backups)?;
            let backup = backups.read_file(&crate::paths::record_name(
                &revision.key,
                &revision.leaf,
                revision.number,
                'b',
            ))?;
            let Some(_) = &revision.diff else {
                return Ok(backup);
            };
            let edits = trusted_config_child(&root, &self.config.root, &self.config.edits)?;
            let diff = edits.read_file(&crate::paths::record_name(
                &revision.key,
                &revision.leaf,
                revision.number,
                'd',
            ))?;
            render_bytes_with_patch(&backup, &diff)
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            let Some(diff) = &revision.diff else {
                return fs::read(&revision.backup);
            };
            render_with_patch(&self.config.root, &revision.backup, diff)
        }
    }

    pub fn backup(&self, revision: &Revision) -> io::Result<Vec<u8>> {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            let root = TrustedDir::open_absolute(&self.config.root)?;
            let backups = trusted_config_child(&root, &self.config.root, &self.config.backups)?;
            backups.read_file(&crate::paths::record_name(
                &revision.key,
                &revision.leaf,
                revision.number,
                'b',
            ))
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            fs::read(&revision.backup)
        }
    }
}

pub fn parse_access_record(path: &Path) -> io::Result<AccessRecord> {
    parse_access_bytes(&fs::read(path)?)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn trusted_config_child(
    root: &TrustedDir,
    root_path: &Path,
    child_path: &Path,
) -> io::Result<TrustedDir> {
    let relative = child_path.strip_prefix(root_path).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "repository child escapes trusted root: {}",
                child_path.display()
            ),
        )
    })?;
    let mut components = relative.components();
    let name = components
        .next()
        .and_then(|part| part.as_os_str().to_str())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid repository child"))?;
    if components.next().is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "repository record directories must be direct children of the trusted root",
        ));
    }
    root.child_dir(name)
}

pub(crate) fn parse_access_bytes(content: &[u8]) -> io::Result<AccessRecord> {
    let line = content.split(|b| *b == b'\n').next().unwrap_or(&[]);
    let normalized: Vec<u8> = line
        .iter()
        .map(|byte| if *byte == 0 { b'\t' } else { *byte })
        .collect();
    let text = String::from_utf8(normalized)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let mut fields = text.splitn(4, '\t');
    let epoch = fields
        .next()
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid access epoch"))?;
    let stamp = fields
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing access stamp"))?;
    let file = fields
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing access path"))?;
    let editor = fields
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing access editor"))?;
    Ok(AccessRecord {
        epoch,
        stamp: stamp.to_owned(),
        path: PathBuf::from(file),
        editor: editor.to_owned(),
    })
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn render_with_patch(root: &Path, backup: &Path, diff: &Path) -> io::Result<Vec<u8>> {
    let temporary = root.join(format!(".rust-render-{}", render_nonce()));
    fs::create_dir_all(&temporary)?;
    let output = temporary.join("out");
    let status = Command::new("patch")
        .args(["-s", "-N", "-o"])
        .arg(&output)
        .arg(backup)
        .arg(diff)
        .status();
    let rendered = match status {
        Ok(status) if status.success() => fs::read(&output),
        Ok(status) => Err(io::Error::other(format!("patch exited with {status}"))),
        Err(error) => Err(error),
    };
    let _ = fs::remove_dir_all(&temporary);
    rendered
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn render_bytes_with_patch(backup: &[u8], diff: &[u8]) -> io::Result<Vec<u8>> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let temporary = std::env::temp_dir().join(format!(".bedit-render-{}", render_nonce()));
    fs::create_dir(&temporary)?;
    let write = |name: &str, bytes: &[u8]| -> io::Result<PathBuf> {
        let path = temporary.join(name);
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(&path)?;
        file.write_all(bytes)?;
        Ok(path)
    };
    let backup_path = write("backup", backup)?;
    let diff_path = write("diff", diff)?;
    let output = temporary.join("out");
    let status = Command::new("patch")
        .args(["-s", "-N", "-o"])
        .arg(&output)
        .arg(backup_path)
        .arg(diff_path)
        .status();
    let rendered = match status {
        Ok(status) if status.success() => fs::read(&output),
        Ok(status) => Err(io::Error::other(format!("patch exited with {status}"))),
        Err(error) => Err(error),
    };
    let _ = fs::remove_dir_all(&temporary);
    rendered
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use super::*;

    #[test]
    fn concurrent_render_scratch_names_do_not_collide() {
        let workers: Vec<_> = (0..32)
            .map(|_| {
                std::thread::spawn(|| {
                    render_bytes_with_patch(
                        b"before\n",
                        b"--- before\n+++ after\n@@ -1 +1 @@\n-before\n+after\n",
                    )
                    .unwrap()
                })
            })
            .collect();
        for worker in workers {
            assert_eq!(worker.join().unwrap(), b"after\n");
        }
    }
}
