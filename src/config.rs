use std::collections::HashMap;
use std::env;
use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use crate::identity::{runtime_identity, system_config_path, RuntimeIdentity, SudoHistoryPolicy};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub root: PathBuf,
    pub access: PathBuf,
    pub backups: PathBuf,
    pub edits: PathBuf,
    pub dirs: PathBuf,
    pub tags: PathBuf,
    pub actors: PathBuf,
    pub actor: String,
    pub history_owner: String,
    pub ownership: Option<(u32, u32)>,
    pub keep_backup_if_no_edit: bool,
    pub diff_tail_lines: usize,
    pub exclude: String,
}

impl Config {
    pub fn from_environment() -> io::Result<Self> {
        Ok(Self::targets_from_environment()?.0)
    }

    pub fn targets_from_environment() -> io::Result<(Self, Option<Self>)> {
        let (identity, policy) = runtime_identity(&system_config_path())?;
        let inherited_home = env::var_os("HOME").map(PathBuf::from);
        let allow_sudo_test_override = cfg!(debug_assertions)
            && env::var_os("BEDIT_TESTING").as_deref() == Some(std::ffi::OsStr::new("1"));
        targets_with_identity(
            &identity,
            policy,
            inherited_home.as_deref(),
            env::var_os("BEDIT_HOME").as_deref(),
            allow_sudo_test_override,
        )
    }

    pub fn load(root: PathBuf, rc_path: &Path, home: &Path) -> io::Result<Self> {
        let user = std::process::Command::new("id")
            .arg("-un")
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
            .unwrap_or_default();
        Self::load_with_identity(root, rc_path, home, user.clone(), user, None)
    }

    fn load_with_identity(
        root: PathBuf,
        rc_path: &Path,
        home: &Path,
        actor: String,
        history_owner: String,
        ownership: Option<(u32, u32)>,
    ) -> io::Result<Self> {
        let mut values = HashMap::new();
        if rc_path.is_file() {
            for line in fs::read_to_string(rc_path)?.lines() {
                let line = line.trim().trim_end_matches('\r');
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                let Some((key, value)) = line.split_once('=') else {
                    continue;
                };
                if key.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
                    && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                {
                    values.insert(key.to_owned(), expand_home(value.trim(), home));
                }
            }
        }

        let location = |name: &str, default: &str| {
            values
                .get(name)
                .map(PathBuf::from)
                .unwrap_or_else(|| root.join(default))
        };

        let access = location("Access", "access");
        let backups = location("Backups", "backups");
        let edits = location("Edits", "edits");
        let dirs = location("Dirs", "dirs");
        let tags = location("Tags", "tags");
        let actors = root.join("actors");

        Ok(Self {
            root,
            access,
            backups,
            edits,
            dirs,
            tags,
            actors,
            actor,
            history_owner,
            ownership,
            keep_backup_if_no_edit: values.get("keepBackupIfNoEdit").is_none_or(|v| v != "0"),
            diff_tail_lines: values
                .get("DiffTailLines")
                .and_then(|v| v.parse().ok())
                .unwrap_or(20),
            exclude: values
                .get("exclude")
                .cloned()
                .unwrap_or_else(|| "*.log,*::_::a,*::_::b,*::_::d".to_owned()),
        })
    }

    pub fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        self.validate_path_topology(path)?;
        let mut missing = Vec::new();
        let mut cursor = path;
        while !cursor.exists() {
            missing.push(cursor.to_path_buf());
            let Some(parent) = cursor.parent() else { break };
            cursor = parent;
        }
        fs::create_dir_all(path)?;
        self.validate_path_topology(path)?;
        for created in missing.into_iter().rev() {
            self.own(&created)?;
        }
        Ok(())
    }

    pub fn validate_repository_topology(&self) -> io::Result<()> {
        for path in [
            &self.root,
            &self.access,
            &self.backups,
            &self.edits,
            &self.dirs,
            &self.tags,
            &self.actors,
        ] {
            if self.ownership.is_some() && !path.starts_with(&self.root) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("repository path escapes root: {}", path.display()),
                ));
            }
            self.validate_path_topology(path)?;
        }
        Ok(())
    }

    fn validate_path_topology(&self, path: &Path) -> io::Result<()> {
        let mut cursor = PathBuf::new();
        for component in path.components() {
            cursor.push(component.as_os_str());
            match fs::symlink_metadata(&cursor) {
                Ok(metadata) => {
                    if metadata.file_type().is_symlink() {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("unsafe symlink in repository path: {}", cursor.display()),
                        ));
                    }
                    if cursor != path && !metadata.is_dir() {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("non-directory repository component: {}", cursor.display()),
                        ));
                    }
                    if cursor.starts_with(&self.root) {
                        if let Some((uid, _)) = self.ownership {
                            if metadata.uid() != uid {
                                return Err(io::Error::new(
                                    io::ErrorKind::PermissionDenied,
                                    format!(
                                        "repository component is not owned by uid {uid}: {}",
                                        cursor.display()
                                    ),
                                ));
                            }
                        }
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    pub fn own(&self, path: &Path) -> io::Result<()> {
        let Some((uid, gid)) = self.ownership else {
            return Ok(());
        };
        use std::os::unix::ffi::OsStrExt;
        let path = std::ffi::CString::new(path.as_os_str().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))?;
        if unsafe { libc::chown(path.as_ptr(), uid, gid) } == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    pub fn own_symlink(&self, path: &Path) -> io::Result<()> {
        let Some((uid, gid)) = self.ownership else {
            return Ok(());
        };
        use std::os::unix::ffi::OsStrExt;
        let path = std::ffi::CString::new(path.as_os_str().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))?;
        if unsafe { libc::lchown(path.as_ptr(), uid, gid) } == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
}

fn targets_with_identity(
    identity: &RuntimeIdentity,
    policy: SudoHistoryPolicy,
    inherited_home: Option<&Path>,
    override_root: Option<&std::ffi::OsStr>,
    allow_sudo_test_override: bool,
) -> io::Result<(Config, Option<Config>)> {
    let home = repository_home(identity, inherited_home);
    let root = repository_root(identity, &home, override_root, allow_sudo_test_override);
    let canonical = Config::load_with_identity(
        root,
        &home.join(".bedit"),
        &home,
        identity.actor.clone(),
        identity.history_owner.name.clone(),
        None,
    )?;
    let mirror = if identity.sudo && policy == SudoHistoryPolicy::RootAndUser {
        let invoker = identity.invoker.as_ref().expect("validated sudo invoker");
        Some(Config::load_with_identity(
            invoker.home.join("bedit"),
            &invoker.home.join(".bedit"),
            &invoker.home,
            identity.actor.clone(),
            invoker.name.clone(),
            Some((invoker.uid, invoker.gid)),
        )?)
    } else {
        None
    };
    Ok((canonical, mirror))
}

fn repository_home(identity: &RuntimeIdentity, inherited_home: Option<&Path>) -> PathBuf {
    if identity.sudo || identity.effective.uid == 0 {
        identity.history_owner.home.clone()
    } else {
        inherited_home
            .map(Path::to_path_buf)
            .unwrap_or_else(|| identity.history_owner.home.clone())
    }
}

fn repository_root(
    identity: &RuntimeIdentity,
    history_home: &Path,
    override_root: Option<&std::ffi::OsStr>,
    allow_sudo_test_override: bool,
) -> PathBuf {
    if !identity.sudo || allow_sudo_test_override {
        if let Some(root) = override_root {
            return PathBuf::from(root);
        }
    }
    history_home.join("bedit")
}

fn expand_home(value: &str, home: &Path) -> String {
    if value == "~" {
        return home.to_string_lossy().into_owned();
    }
    if let Some(rest) = value.strip_prefix("~/") {
        return home.join(rest).to_string_lossy().into_owned();
    }
    value.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::Account;

    #[test]
    fn loads_legacy_locations_and_values() {
        let base = std::env::temp_dir().join(format!("bedit-config-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        let rc = base.join(".bedit");
        fs::write(
            &rc,
            "Access=~/custom-access\nDiffTailLines=7\nkeepBackupIfNoEdit=0\n",
        )
        .unwrap();
        let config = Config::load(base.join("repo"), &rc, &base).unwrap();
        assert_eq!(config.access, base.join("custom-access"));
        assert_eq!(config.diff_tail_lines, 7);
        assert!(!config.keep_backup_if_no_edit);
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn sudo_and_direct_root_ignore_inherited_home() {
        let root = Account {
            name: "root".into(),
            uid: 0,
            gid: 0,
            home: "/root-real".into(),
        };
        let luke = Account {
            name: "luke".into(),
            uid: 1001,
            gid: 1001,
            home: "/srv/luke".into(),
        };
        let sudo_user_policy = RuntimeIdentity {
            effective: root.clone(),
            history_owner: luke,
            invoker: None,
            actor: "luke".into(),
            sudo: true,
        };
        assert_eq!(
            repository_home(&sudo_user_policy, Some(Path::new("/preserved/home"))),
            PathBuf::from("/srv/luke")
        );
        assert_eq!(
            repository_root(
                &sudo_user_policy,
                Path::new("/srv/luke"),
                Some(std::ffi::OsStr::new("/preserved/store")),
                false,
            ),
            PathBuf::from("/srv/luke/bedit")
        );
        let direct_root = RuntimeIdentity {
            effective: root.clone(),
            history_owner: root,
            invoker: None,
            actor: "root".into(),
            sudo: false,
        };
        assert_eq!(
            repository_home(&direct_root, Some(Path::new("/misleading/home"))),
            PathBuf::from("/root-real")
        );
        let (root_target, mirror) = targets_with_identity(
            &direct_root,
            SudoHistoryPolicy::RootAndUser,
            Some(Path::new("/misleading/home")),
            None,
            false,
        )
        .unwrap();
        assert_eq!(root_target.root, PathBuf::from("/root-real/bedit"));
        assert_eq!(root_target.actor, "root");
        assert!(mirror.is_none());

        let ordinary = RuntimeIdentity {
            effective: Account {
                name: "luke".into(),
                uid: 1001,
                gid: 1001,
                home: "/srv/luke".into(),
            },
            history_owner: Account {
                name: "luke".into(),
                uid: 1001,
                gid: 1001,
                home: "/srv/luke".into(),
            },
            invoker: None,
            actor: "luke".into(),
            sudo: false,
        };
        let (user_target, mirror) = targets_with_identity(
            &ordinary,
            SudoHistoryPolicy::RootAndUser,
            Some(Path::new("/session/home")),
            None,
            false,
        )
        .unwrap();
        assert_eq!(user_target.root, PathBuf::from("/session/home/bedit"));
        assert_eq!(user_target.actor, "luke");
        assert!(mirror.is_none());
    }

    #[test]
    fn sudo_targets_are_root_canonical_and_optional_owned_user_mirror() {
        let root = Account {
            name: "root".into(),
            uid: 0,
            gid: 0,
            home: "/root-real".into(),
        };
        let faf = Account {
            name: "faf".into(),
            uid: 1002,
            gid: 1002,
            home: "/srv/faf".into(),
        };
        let identity = RuntimeIdentity {
            effective: root.clone(),
            history_owner: root,
            invoker: Some(faf),
            actor: "faf".into(),
            sudo: true,
        };
        let (canonical, mirror) = targets_with_identity(
            &identity,
            SudoHistoryPolicy::RootAndUser,
            Some(Path::new("/spoofed/home")),
            None,
            false,
        )
        .unwrap();
        assert_eq!(canonical.root, PathBuf::from("/root-real/bedit"));
        assert_eq!(canonical.actor, "faf");
        let mirror = mirror.unwrap();
        assert_eq!(mirror.root, PathBuf::from("/srv/faf/bedit"));
        assert_eq!(mirror.ownership, Some((1002, 1002)));
        assert_eq!(mirror.actor, "faf");

        let (_, mirror) =
            targets_with_identity(&identity, SudoHistoryPolicy::RootOnly, None, None, false)
                .unwrap();
        assert!(mirror.is_none());
    }
}
