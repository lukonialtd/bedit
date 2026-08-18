use std::ffi::OsString;
use std::fs::File;
use std::io;
use std::path::Path;

pub struct TrustedDir;

fn unsupported<T>() -> io::Result<T> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "trusted descriptor filesystem operations are currently supported only on Linux",
    ))
}

impl TrustedDir {
    pub fn open_or_create_absolute(_: &Path, _: u32) -> io::Result<Self> {
        unsupported()
    }

    pub fn child_dir_create(&self, _: &str, _: u32) -> io::Result<Self> {
        unsupported()
    }

    pub fn entries(&self) -> io::Result<Vec<OsString>> {
        unsupported()
    }

    pub fn open_regular(&self, _: &str, _: bool) -> io::Result<File> {
        unsupported()
    }

    pub fn open_or_create_regular(&self, _: &str, _: u32) -> io::Result<File> {
        unsupported()
    }

    pub fn read_file(&self, _: &str) -> io::Result<Vec<u8>> {
        unsupported()
    }

    pub fn write_noreplace(&self, _: &str, _: &str, _: &[u8], _: u32) -> io::Result<()> {
        unsupported()
    }

    pub fn write_replace(&self, _: &str, _: &str, _: &[u8], _: u32) -> io::Result<()> {
        unsupported()
    }

    pub fn symlink_noreplace(&self, _: &str, _: &Path) -> io::Result<()> {
        unsupported()
    }

    pub fn unlink_file(&self, _: &str) -> io::Result<()> {
        unsupported()
    }
}

#[cfg(test)]
mod tests {
    use super::TrustedDir;
    use std::io::ErrorKind;
    use std::path::Path;

    #[test]
    fn unsupported_operations_fail_closed() {
        let error = TrustedDir::open_or_create_absolute(Path::new("/tmp/bedit"), 0o700)
            .err()
            .expect("non-Linux trusted mutation must fail");
        assert_eq!(error.kind(), ErrorKind::Unsupported);
        assert_eq!(
            error.to_string(),
            "trusted descriptor filesystem operations are currently supported only on Linux"
        );
    }
}
