use std::env;
use std::ffi::{CStr, CString};
use std::fs;
use std::io;
use std::mem::MaybeUninit;
use std::path::Path;
use std::path::PathBuf;
use std::ptr;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SudoHistoryPolicy {
    RootAndUser,
    RootOnly,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    pub name: String,
    pub uid: u32,
    pub gid: u32,
    pub home: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionContext {
    pub effective_uid: u32,
    pub sudo_user: Option<String>,
    pub sudo_uid: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeIdentity {
    pub effective: Account,
    pub history_owner: Account,
    pub invoker: Option<Account>,
    pub actor: String,
    pub sudo: bool,
}

pub trait AccountLookup {
    fn by_uid(&self, uid: u32) -> io::Result<Account>;
    fn by_name(&self, name: &str) -> io::Result<Account>;
}

pub fn resolve_identity(
    context: &ExecutionContext,
    _policy: SudoHistoryPolicy,
    accounts: &impl AccountLookup,
) -> io::Result<RuntimeIdentity> {
    let effective = accounts.by_uid(context.effective_uid)?;
    let sudo_actor = if context.effective_uid == 0 {
        context
            .sudo_uid
            .as_deref()
            .and_then(|value| value.parse::<u32>().ok())
            .filter(|uid| *uid != 0)
            .and_then(|uid| accounts.by_uid(uid).ok())
            .filter(|account| context.sudo_user.as_deref() == Some(account.name.as_str()))
    } else {
        None
    };
    let (history_owner, invoker, actor, sudo) = match sudo_actor {
        Some(invoker) => {
            let actor = invoker.name.clone();
            (effective.clone(), Some(invoker), actor, true)
        }
        None => (effective.clone(), None, effective.name.clone(), false),
    };
    Ok(RuntimeIdentity {
        effective,
        history_owner,
        invoker,
        actor,
        sudo,
    })
}

pub fn parse_sudo_history_config(contents: &str) -> io::Result<SudoHistoryPolicy> {
    let mut found = None;
    for (index, raw) in contents.lines().enumerate() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            if line.starts_with("sudo_history") {
                return Err(invalid_policy(index + 1, "expected key = value"));
            }
            continue;
        };
        if key.trim() != "sudo_history" {
            continue;
        }
        if found.is_some() {
            return Err(invalid_policy(index + 1, "duplicate sudo_history setting"));
        }
        found = Some(match value.trim() {
            "\"root_and_user\"" | "\"user\"" => SudoHistoryPolicy::RootAndUser,
            "\"root_only\"" | "\"root\"" => SudoHistoryPolicy::RootOnly,
            _ => {
                return Err(invalid_policy(
                    index + 1,
                    "expected \"root_and_user\" or \"root_only\"",
                ))
            }
        });
    }
    Ok(found.unwrap_or(SudoHistoryPolicy::RootAndUser))
}

fn invalid_policy(line: usize, detail: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("invalid sudo_history policy at line {line}: {detail}"),
    )
}

pub struct SystemAccounts;

impl AccountLookup for SystemAccounts {
    fn by_uid(&self, uid: u32) -> io::Result<Account> {
        lookup(uid, None)
    }
    fn by_name(&self, name: &str) -> io::Result<Account> {
        lookup(0, Some(name))
    }
}

fn lookup(uid: u32, name: Option<&str>) -> io::Result<Account> {
    let mut pwd = MaybeUninit::<libc::passwd>::uninit();
    let mut result = ptr::null_mut();
    let size = unsafe { libc::sysconf(libc::_SC_GETPW_R_SIZE_MAX) };
    let mut buffer = vec![0_u8; if size > 0 { size as usize } else { 16_384 }];
    let code = if let Some(name) = name {
        let name = CString::new(name).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "account name contains NUL")
        })?;
        unsafe {
            libc::getpwnam_r(
                name.as_ptr(),
                pwd.as_mut_ptr(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut result,
            )
        }
    } else {
        unsafe {
            libc::getpwuid_r(
                uid,
                pwd.as_mut_ptr(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut result,
            )
        }
    };
    if code != 0 {
        return Err(io::Error::from_raw_os_error(code));
    }
    if result.is_null() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "system account not found",
        ));
    }
    let pwd = unsafe { pwd.assume_init() };
    let text = |value: *const libc::c_char| {
        unsafe { CStr::from_ptr(value) }
            .to_string_lossy()
            .into_owned()
    };
    Ok(Account {
        name: text(pwd.pw_name),
        uid: pwd.pw_uid,
        gid: pwd.pw_gid,
        home: PathBuf::from(text(pwd.pw_dir)),
    })
}

pub fn runtime_identity(config_path: &Path) -> io::Result<(RuntimeIdentity, SudoHistoryPolicy)> {
    let policy = match fs::read_to_string(config_path) {
        Ok(contents) => parse_sudo_history_config(&contents)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => SudoHistoryPolicy::RootAndUser,
        Err(error) => {
            return Err(io::Error::new(
                error.kind(),
                format!("cannot read {}: {error}", config_path.display()),
            ))
        }
    };
    let context = ExecutionContext {
        effective_uid: unsafe { libc::geteuid() },
        sudo_user: env::var("SUDO_USER").ok(),
        sudo_uid: env::var("SUDO_UID").ok(),
    };
    Ok((resolve_identity(&context, policy, &SystemAccounts)?, policy))
}

pub fn system_config_path() -> PathBuf {
    if cfg!(debug_assertions)
        && env::var_os("BEDIT_TESTING").as_deref() == Some(std::ffi::OsStr::new("1"))
    {
        if let Some(path) = env::var_os("BEDIT_SYSTEM_CONFIG") {
            return PathBuf::from(path);
        }
    }
    PathBuf::from("/etc/bedit/config.toml")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    struct Accounts(HashMap<u32, Account>);
    impl Accounts {
        fn new(items: Vec<Account>) -> Self {
            Self(items.into_iter().map(|a| (a.uid, a)).collect())
        }
    }
    impl AccountLookup for Accounts {
        fn by_uid(&self, uid: u32) -> io::Result<Account> {
            self.0
                .get(&uid)
                .cloned()
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "missing account"))
        }
        fn by_name(&self, name: &str) -> io::Result<Account> {
            self.0
                .values()
                .find(|a| a.name == name)
                .cloned()
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "missing account"))
        }
    }
    fn account(name: &str, uid: u32, home: &str) -> Account {
        Account {
            name: name.into(),
            uid,
            gid: uid,
            home: home.into(),
        }
    }
    fn fixture() -> Accounts {
        Accounts::new(vec![
            account("root", 0, "/root"),
            account("luke", 1001, "/srv/unusual/luke"),
            account("pat", 1002, "/home/pat"),
        ])
    }

    #[test]
    fn ordinary_user_owns_and_authors_normal_history() {
        let got = resolve_identity(
            &ExecutionContext {
                effective_uid: 1001,
                sudo_user: None,
                sudo_uid: None,
            },
            SudoHistoryPolicy::RootAndUser,
            &fixture(),
        )
        .unwrap();
        assert_eq!(
            (
                got.invoker.as_ref().map(|account| account.name.as_str()),
                got.actor.as_str(),
                got.sudo
            ),
            (None, "luke", false)
        );
    }
    #[test]
    fn direct_root_ignores_stray_sudo_user_without_matching_uid_context() {
        let got = resolve_identity(
            &ExecutionContext {
                effective_uid: 0,
                sudo_user: Some("luke".into()),
                sudo_uid: None,
            },
            SudoHistoryPolicy::RootAndUser,
            &fixture(),
        )
        .unwrap();
        assert_eq!(
            (
                got.invoker.as_ref().map(|account| account.name.as_str()),
                got.actor.as_str(),
                got.sudo
            ),
            (None, "root", false)
        );
    }
    #[test]
    fn mismatched_sudo_name_and_uid_are_not_trusted() {
        let got = resolve_identity(
            &ExecutionContext {
                effective_uid: 0,
                sudo_user: Some("luke".into()),
                sudo_uid: Some("1002".into()),
            },
            SudoHistoryPolicy::RootAndUser,
            &fixture(),
        )
        .unwrap();
        assert_eq!(
            (
                got.invoker.as_ref().map(|account| account.name.as_str()),
                got.actor.as_str(),
                got.sudo
            ),
            (None, "root", false)
        );
    }
    #[test]
    fn sudo_root_policy_consolidates_history_and_preserves_actor() {
        let got = resolve_identity(
            &ExecutionContext {
                effective_uid: 0,
                sudo_user: Some("luke".into()),
                sudo_uid: Some("1001".into()),
            },
            SudoHistoryPolicy::RootAndUser,
            &fixture(),
        )
        .unwrap();
        assert_eq!(
            (
                got.invoker.as_ref().map(|account| account.name.as_str()),
                got.actor.as_str(),
                got.sudo
            ),
            (Some("luke"), "luke", true)
        );
    }
    #[test]
    fn sudo_root_only_still_retains_validated_invoker() {
        let got = resolve_identity(
            &ExecutionContext {
                effective_uid: 0,
                sudo_user: Some("luke".into()),
                sudo_uid: Some("1001".into()),
            },
            SudoHistoryPolicy::RootOnly,
            &fixture(),
        )
        .unwrap();
        assert_eq!(
            got.invoker.as_ref().unwrap().home,
            PathBuf::from("/srv/unusual/luke")
        );
        assert_eq!((got.actor.as_str(), got.sudo), ("luke", true));
    }
    #[test]
    fn parses_default_and_both_policy_values() {
        assert_eq!(
            parse_sudo_history_config("").unwrap(),
            SudoHistoryPolicy::RootAndUser
        );
        assert_eq!(
            parse_sudo_history_config("# policy\nsudo_history = \"root\"\n").unwrap(),
            SudoHistoryPolicy::RootOnly
        );
        assert_eq!(
            parse_sudo_history_config("other = \"kept\"\nsudo_history = \"user\"\n").unwrap(),
            SudoHistoryPolicy::RootAndUser
        );
        assert_eq!(
            parse_sudo_history_config("sudo_history = \"root_only\"\n").unwrap(),
            SudoHistoryPolicy::RootOnly
        );
        assert_eq!(
            parse_sudo_history_config("sudo_history = \"root_and_user\"\n").unwrap(),
            SudoHistoryPolicy::RootAndUser
        );
    }
    #[test]
    fn malformed_or_unknown_policy_fails_instead_of_misrouting() {
        for text in [
            "sudo_history = \"wheel\"\n",
            "sudo_history = user\n",
            "sudo_history = \"root\"\nsudo_history = \"user\"\n",
        ] {
            assert!(parse_sudo_history_config(text)
                .unwrap_err()
                .to_string()
                .contains("sudo_history"));
        }
    }
}
