use rustix::fd::OwnedFd;
use rustix::fs::{self, AtFlags, Mode, OFlags};
#[cfg(target_os = "linux")]
use rustix::fs::{RenameFlags, ResolveFlags};
#[cfg(target_os = "macos")]
use std::ffi::CString;
use std::ffi::OsString;
use std::io::{self, Read, Write};
#[cfg(target_os = "macos")]
use std::os::fd::AsRawFd;
use std::os::fd::{AsFd, FromRawFd, IntoRawFd};
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path};

pub struct TrustedDir {
    fd: OwnedFd,
}

fn component(name: &str) -> io::Result<&str> {
    if name.is_empty() || matches!(name, "." | "..") || name.contains('/') {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid path component",
        ))
    } else {
        Ok(name)
    }
}

fn open_component<Fd: AsFd>(
    dir: &Fd,
    name: &str,
    flags: OFlags,
    mode: Mode,
) -> io::Result<OwnedFd> {
    let name = component(name)?;
    #[cfg(target_os = "linux")]
    {
        fs::openat2(
            dir,
            name,
            flags,
            mode,
            ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_MAGICLINKS,
        )
        .map_err(Into::into)
    }
    #[cfg(target_os = "macos")]
    {
        // `name` is exactly one component and `dir` is already trusted. O_NOFOLLOW
        // rejects a symlink in that only unresolved component.
        fs::openat(dir, name, flags, mode).map_err(Into::into)
    }
}

fn rename_noreplace_at(
    old_dir: &OwnedFd,
    old_name: &str,
    new_dir: &OwnedFd,
    new_name: &str,
) -> io::Result<()> {
    let old_name = component(old_name)?;
    let new_name = component(new_name)?;
    #[cfg(target_os = "linux")]
    {
        fs::renameat_with(old_dir, old_name, new_dir, new_name, RenameFlags::NOREPLACE)
            .map_err(Into::into)
    }
    #[cfg(target_os = "macos")]
    {
        let old_name = CString::new(old_name.as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid source name"))?;
        let new_name = CString::new(new_name.as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid destination name"))?;
        // SAFETY: both C strings are NUL-terminated and live for the call; both
        // raw descriptors are borrowed and valid. RENAME_EXCL makes publication
        // atomically fail when the destination exists.
        let result = unsafe {
            libc::renameatx_np(
                old_dir.as_raw_fd(),
                old_name.as_ptr(),
                new_dir.as_raw_fd(),
                new_name.as_ptr(),
                libc::RENAME_EXCL,
            )
        };
        if result == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
}

impl TrustedDir {
    pub fn open_absolute(path: &std::path::Path) -> io::Result<Self> {
        let relative = path.strip_prefix("/").map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "trusted root must be absolute")
        })?;
        let root = fs::open(
            "/",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        )?;
        let mut current = Self { fd: root };
        for item in relative.components() {
            let Component::Normal(item) = item else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "invalid absolute directory path",
                ));
            };
            let name = item.to_str().ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "non-UTF-8 directory component")
            })?;
            current = current.child_dir(name)?;
        }
        Ok(current)
    }

    pub fn open_or_create_absolute(path: &Path, mode: u32) -> io::Result<Self> {
        let relative = path.strip_prefix("/").map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "trusted root must be absolute")
        })?;
        let root = fs::open(
            "/",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        )?;
        let mut current = Self { fd: root };
        for item in relative.components() {
            let Component::Normal(item) = item else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "invalid absolute directory path",
                ));
            };
            let name = item.to_str().ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "non-UTF-8 directory component")
            })?;
            current = current.child_dir_create(name, mode)?;
        }
        Ok(current)
    }

    pub fn child_dir(&self, name: &str) -> io::Result<Self> {
        let name = component(name)?;
        let fd = open_component(
            &self.fd,
            name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        )?;
        Ok(Self { fd })
    }

    pub fn child_dir_create(&self, name: &str, mode: u32) -> io::Result<Self> {
        let name = component(name)?;
        match fs::mkdirat(&self.fd, name, Mode::from_raw_mode(mode as _)) {
            Ok(()) => {}
            Err(rustix::io::Errno::EXIST) => {}
            Err(error) => return Err(error.into()),
        }
        self.child_dir(name)
    }

    pub fn descendant_dir_create(&self, relative: &Path, mode: u32) -> io::Result<Self> {
        let mut current = Self {
            fd: fs::openat(
                &self.fd,
                ".",
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
                Mode::empty(),
            )?,
        };
        for item in relative.components() {
            let Component::Normal(item) = item else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "invalid relative directory path",
                ));
            };
            let name = item.to_str().ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "non-UTF-8 directory component")
            })?;
            current = current.child_dir_create(name, mode)?;
        }
        Ok(current)
    }

    pub fn create_file(&self, name: &str, mode: u32) -> io::Result<std::fs::File> {
        let fd = open_component(
            &self.fd,
            component(name)?,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(mode as _),
        )?;
        let stat = fs::fstat(&fd)?;
        if fs::FileType::from_raw_mode(stat.st_mode) != fs::FileType::RegularFile
            || stat.st_nlink != 1
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "created inode is not a single-link regular file",
            ));
        }
        // SAFETY: ownership is transferred exactly once from OwnedFd to File.
        Ok(unsafe { std::fs::File::from_raw_fd(fd.into_raw_fd()) })
    }

    pub fn open_regular(&self, name: &str, writable: bool) -> io::Result<std::fs::File> {
        self.open_regular_with_links(name, writable, true)
    }

    pub fn open_source_regular(&self, name: &str) -> io::Result<std::fs::File> {
        self.open_regular_with_links(name, false, false)
    }

    fn open_regular_with_links(
        &self,
        name: &str,
        writable: bool,
        require_single_link: bool,
    ) -> io::Result<std::fs::File> {
        let access = if writable {
            OFlags::RDWR
        } else {
            OFlags::RDONLY
        };
        let fd = open_component(
            &self.fd,
            component(name)?,
            access | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        let stat = fs::fstat(&fd)?;
        if fs::FileType::from_raw_mode(stat.st_mode) != fs::FileType::RegularFile
            || (require_single_link && stat.st_nlink != 1)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "inode is not a single-link regular file",
            ));
        }
        // SAFETY: ownership is transferred exactly once from OwnedFd to File.
        Ok(unsafe { std::fs::File::from_raw_fd(fd.into_raw_fd()) })
    }

    pub fn open_or_create_regular(&self, name: &str, mode: u32) -> io::Result<std::fs::File> {
        match self.create_file(name, mode) {
            Ok(file) => Ok(file),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                self.open_regular(name, true)
            }
            Err(error) => Err(error),
        }
    }

    pub fn read_file(&self, name: &str) -> io::Result<Vec<u8>> {
        let mut bytes = Vec::new();
        self.open_regular(name, false)?.read_to_end(&mut bytes)?;
        Ok(bytes)
    }

    pub fn entries(&self) -> io::Result<Vec<OsString>> {
        let mut result = Vec::new();
        for entry in fs::Dir::read_from(&self.fd)? {
            let entry = entry?;
            let bytes = entry.file_name().to_bytes();
            if bytes != b"." && bytes != b".." {
                result.push(OsString::from_vec(bytes.to_vec()));
            }
        }
        Ok(result)
    }

    pub fn write_noreplace(
        &self,
        final_name: &str,
        staging_name: &str,
        bytes: &[u8],
        mode: u32,
    ) -> io::Result<()> {
        let final_name = component(final_name)?;
        let staging_name = component(staging_name)?;
        let mut file = self.create_file(staging_name, mode)?;
        let result = (|| {
            file.write_all(bytes)?;
            file.sync_all()?;
            rename_noreplace_at(&self.fd, staging_name, &self.fd, final_name)?;
            fs::fsync(&self.fd)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::unlinkat(&self.fd, staging_name, fs::AtFlags::empty());
        }
        result
    }

    pub fn write_replace(
        &self,
        final_name: &str,
        staging_name: &str,
        bytes: &[u8],
        mode: u32,
    ) -> io::Result<()> {
        let final_name = component(final_name)?;
        let staging_name = component(staging_name)?;
        match fs::statat(&self.fd, final_name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat)
                if fs::FileType::from_raw_mode(stat.st_mode) == fs::FileType::RegularFile
                    && stat.st_nlink == 1 => {}
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "replacement destination is not a single-link regular file",
                ))
            }
            Err(rustix::io::Errno::NOENT) => {}
            Err(error) => return Err(error.into()),
        }
        let mut file = self.create_file(staging_name, mode)?;
        let result = (|| {
            file.set_permissions(std::fs::Permissions::from_mode(mode))?;
            file.write_all(bytes)?;
            file.sync_all()?;
            fs::renameat(&self.fd, staging_name, &self.fd, final_name)?;
            fs::fsync(&self.fd)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::unlinkat(&self.fd, staging_name, AtFlags::empty());
        }
        result
    }

    pub fn symlink_noreplace(&self, name: &str, target: &Path) -> io::Result<()> {
        fs::symlinkat(target, &self.fd, component(name)?)?;
        fs::fsync(&self.fd)?;
        Ok(())
    }

    pub fn read_link(&self, name: &str) -> io::Result<std::path::PathBuf> {
        let target = fs::readlinkat(&self.fd, component(name)?, Vec::new())?;
        Ok(std::path::PathBuf::from(OsString::from_vec(
            target.into_bytes(),
        )))
    }

    pub fn stat_nofollow(&self, name: &str) -> io::Result<fs::Stat> {
        fs::statat(&self.fd, component(name)?, AtFlags::SYMLINK_NOFOLLOW).map_err(Into::into)
    }

    pub fn rename_noreplace(&self, source: &str, destination: &str) -> io::Result<()> {
        rename_noreplace_at(&self.fd, source, &self.fd, destination)?;
        self.sync()
    }

    pub fn rename_replace(&self, source: &str, destination: &str) -> io::Result<()> {
        fs::renameat(
            &self.fd,
            component(source)?,
            &self.fd,
            component(destination)?,
        )?;
        self.sync()
    }

    pub fn symlink_replace(&self, name: &str, staging_name: &str, target: &Path) -> io::Result<()> {
        let name = component(name)?;
        let staging_name = component(staging_name)?;
        fs::symlinkat(target, &self.fd, staging_name)?;
        let result =
            fs::renameat(&self.fd, staging_name, &self.fd, name).and_then(|()| fs::fsync(&self.fd));
        if result.is_err() {
            let _ = fs::unlinkat(&self.fd, staging_name, AtFlags::empty());
        }
        result.map_err(Into::into)
    }

    pub fn unlink_file(&self, name: &str) -> io::Result<()> {
        fs::unlinkat(&self.fd, component(name)?, AtFlags::empty())?;
        fs::fsync(&self.fd)?;
        Ok(())
    }

    pub fn remove_empty_dir(&self, name: &str) -> io::Result<()> {
        fs::unlinkat(&self.fd, component(name)?, AtFlags::REMOVEDIR)?;
        self.sync()
    }

    pub fn sync(&self) -> io::Result<()> {
        fs::fsync(&self.fd).map_err(Into::into)
    }

    pub fn as_fd(&self) -> impl AsFd + '_ {
        self.fd.as_fd()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::symlink;

    fn base(name: &str) -> std::path::PathBuf {
        fs::canonicalize(std::env::temp_dir())
            .unwrap()
            .join(format!("bedit-trusted-fd-{name}-{}", std::process::id()))
    }

    #[test]
    fn rejects_symlinked_component_and_final_destination() {
        let base = base("links");
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("repo")).unwrap();
        fs::create_dir_all(base.join("victim")).unwrap();
        fs::write(base.join("victim/sentinel"), b"unchanged").unwrap();
        let root = TrustedDir::open_absolute(&base).unwrap();
        symlink(base.join("victim"), base.join("repo/child")).unwrap();
        let repo = root.child_dir("repo").unwrap();
        assert!(repo.child_dir("child").is_err());
        symlink(base.join("victim/sentinel"), base.join("repo/final")).unwrap();
        assert!(repo
            .write_noreplace("final", ".stage", b"attack", 0o600)
            .is_err());
        symlink(
            base.join("victim/sentinel"),
            base.join("repo/replace-final"),
        )
        .unwrap();
        assert!(repo
            .write_replace("replace-final", ".replace-stage", b"attack", 0o600)
            .is_err());
        assert_eq!(
            fs::read(base.join("victim/sentinel")).unwrap(),
            b"unchanged"
        );
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn opened_directory_survives_pathname_swap_without_redirecting() {
        let base = base("swap");
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("repo/records")).unwrap();
        fs::create_dir_all(base.join("victim")).unwrap();
        let root = TrustedDir::open_absolute(&base).unwrap();
        let repo = root.child_dir("repo").unwrap();
        let records = repo.child_dir("records").unwrap();
        fs::rename(base.join("repo/records"), base.join("detached")).unwrap();
        symlink(base.join("victim"), base.join("repo/records")).unwrap();
        records
            .write_noreplace("record", ".stage", b"safe", 0o600)
            .unwrap();
        assert_eq!(fs::read(base.join("detached/record")).unwrap(), b"safe");
        assert!(fs::read_dir(base.join("victim")).unwrap().next().is_none());
        fs::remove_file(base.join("repo/records")).unwrap();
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn malicious_staging_and_lock_entries_fail_closed() {
        let base = base("staging-lock");
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("repo")).unwrap();
        fs::write(base.join("victim"), b"unchanged").unwrap();
        symlink(base.join("victim"), base.join("repo/.stage")).unwrap();
        symlink(
            base.join("victim"),
            base.join("repo/.bedit-publication.lock"),
        )
        .unwrap();
        let repo = TrustedDir::open_absolute(&base.join("repo")).unwrap();
        assert!(repo
            .write_noreplace("record", ".stage", b"attack", 0o600)
            .is_err());
        assert!(repo
            .open_or_create_regular(".bedit-publication.lock", 0o600)
            .is_err());
        assert_eq!(fs::read(base.join("victim")).unwrap(), b"unchanged");
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn opened_lock_inode_survives_lock_path_swap() {
        let base = base("lock-swap");
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("repo")).unwrap();
        fs::write(base.join("victim"), b"unchanged").unwrap();
        let repo = TrustedDir::open_absolute(&base.join("repo")).unwrap();
        let lock = repo
            .open_or_create_regular(".bedit-publication.lock", 0o600)
            .unwrap();
        fs::rename(
            base.join("repo/.bedit-publication.lock"),
            base.join("detached-lock"),
        )
        .unwrap();
        symlink(
            base.join("victim"),
            base.join("repo/.bedit-publication.lock"),
        )
        .unwrap();
        rustix::fs::flock(&lock, rustix::fs::FlockOperation::LockExclusive).unwrap();
        repo.write_noreplace("record", ".stage", b"safe", 0o600)
            .unwrap();
        assert_eq!(fs::read(base.join("victim")).unwrap(), b"unchanged");
        assert_eq!(fs::read(base.join("repo/record")).unwrap(), b"safe");
        fs::remove_file(base.join("repo/.bedit-publication.lock")).unwrap();
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn rejects_symlinked_root_special_files_and_existing_finals() {
        let base = base("root-special-existing");
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("repo")).unwrap();
        symlink(base.join("repo"), base.join("linked-repo")).unwrap();
        assert!(TrustedDir::open_absolute(&base.join("linked-repo")).is_err());

        let fifo = std::ffi::CString::new(base.join("repo/fifo").as_os_str().as_bytes()).unwrap();
        // SAFETY: `fifo` is a valid, NUL-terminated pathname used only to make
        // an adversarial fixture; no descriptor ownership is transferred.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        let repo = TrustedDir::open_absolute(&base.join("repo")).unwrap();
        assert!(repo.open_regular("fifo", false).is_err());

        fs::write(base.join("repo/final"), b"attacker").unwrap();
        let error = repo
            .write_noreplace("final", ".stage", b"replacement", 0o600)
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(base.join("repo/final")).unwrap(), b"attacker");
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn legitimate_repository_exercises_trusted_dir_api() {
        let base = base("legitimate-api");
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        let root = TrustedDir::open_absolute(&base).unwrap();
        let child = root.child_dir_create("child", 0o700).unwrap();
        child
            .write_noreplace("first", ".first-stage", b"one", 0o600)
            .unwrap();
        assert_eq!(child.read_file("first").unwrap(), b"one");
        assert!(child.entries().unwrap().iter().any(|name| name == "first"));
        assert!(child.stat_nofollow("first").unwrap().st_size == 3);
        child.rename_noreplace("first", "second").unwrap();
        child
            .write_replace("second", ".replace-stage", b"two", 0o600)
            .unwrap();
        assert_eq!(
            child
                .open_source_regular("second")
                .unwrap()
                .metadata()
                .unwrap()
                .len(),
            3
        );
        child
            .symlink_noreplace("link", Path::new("second"))
            .unwrap();
        assert_eq!(child.read_link("link").unwrap(), Path::new("second"));
        child
            .symlink_replace("link", ".link-stage", Path::new("second"))
            .unwrap();
        child.unlink_file("link").unwrap();
        child.unlink_file("second").unwrap();
        child.sync().unwrap();
        root.remove_empty_dir("child").unwrap();
        fs::remove_dir_all(base).unwrap();
    }
}
