use crate::trusted_fs::TrustedDir;
use rustix::fs::{self, FileType, Mode};
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

const NAMES: &[&str] = &["bedit", "bed", "bvi", "bnvim", "bnano", "bpico", "bemacs"];
const LEGACY_EDITORS: &[&str] = &["vi", "nvim", "nano", "pico", "emacs", "ed"];

pub fn main(args: &[String]) -> io::Result<()> {
    if args.len() != 10 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid trusted installer helper arguments",
        ));
    }
    let action = &args[0];
    let prefix = absolute(&args[1])?;
    let config = if args[2] == "-" {
        None
    } else {
        Some(absolute(&args[2])?)
    };
    #[cfg(target_os = "macos")]
    {
        // SAFETY: geteuid has no pointer arguments or ownership effects.
        if unsafe { libc::geteuid() } == 0 || config.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "privileged macOS installation is unsupported; use a non-root user install",
            ));
        }
        let home = std::env::var_os("HOME")
            .map(std::path::PathBuf::from)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "HOME is required"))?;
        if !prefix.starts_with(&home) || prefix == home {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "macOS user install prefix must be below HOME",
            ));
        }
    }
    let mode = match args[3].as_str() {
        "named" | "transparent" => args[3].as_str(),
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid install mode",
            ))
        }
    };
    let policy = match args[4].as_str() {
        "root_and_user" | "root_only" => args[4].as_str(),
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid sudo policy",
            ))
        }
    };
    let source = absolute(&args[5])?;
    let man_root = absolute(&args[6])?;
    let editors = parse_editors(&args[7])?;
    let copy = parse_bool(&args[8])?;
    let purge = parse_bool(&args[9])?;
    match action.as_str() {
        "install" => install(
            prefix, config, mode, policy, source, man_root, &editors, copy,
        ),
        "uninstall" => uninstall(prefix, config, &editors, purge),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid helper action",
        )),
    }
}

#[allow(clippy::too_many_arguments)]
fn install(
    prefix_path: &Path,
    config_path: Option<&Path>,
    mode: &str,
    policy: &str,
    source_path: &Path,
    man_root_path: &Path,
    editors: &[(String, String)],
    copy: bool,
) -> io::Result<()> {
    let source = if copy {
        let source = TrustedDir::open_absolute(source_path)?;
        validate_payload(&source)?;
        Some(source)
    } else {
        None
    };
    let prefix = TrustedDir::open_or_create_absolute(prefix_path, 0o755)?;
    checkpoint("installer-prefix-opened");
    let bin = prefix.child_dir_create("bin", 0o755)?;
    checkpoint("installer-bin-opened");
    let libexec_parent = prefix.child_dir_create("libexec", 0o755)?;
    checkpoint("installer-libexec-opened");
    let libexec = libexec_parent.child_dir_create("bedit", 0o755)?;
    migrate_legacy_install(&bin, &libexec)?;
    let man = prefix
        .child_dir_create("share", 0o755)?
        .child_dir_create("man", 0o755)?
        .child_dir_create("man1", 0o755)?;

    if let Some(source) = &source {
        let release = source_release(source)?;
        let versions = libexec.child_dir_create("versions", 0o755)?;
        let instance = format!("{}-{}-{}", release, std::process::id(), nonce());
        let instance_dir = versions.child_dir_create(&instance, 0o755)?;
        for name in NAMES {
            copy_payload(source, &instance_dir, &format!("{name}-rust"))?;
        }
        replace_managed_link(
            &libexec,
            "current",
            Path::new(&format!("versions/{instance}")),
        )?;
    } else {
        let current = libexec.child_dir("current")?;
        let _ = current.open_regular("bedit-rust", false)?;
    }

    let editor_names = editors
        .iter()
        .map(|(editor, _)| editor.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    libexec.write_replace(
        "install-manifest",
        &stage("manifest"),
        format!(
            "mode={mode}\nversion={}\neditors={editor_names}\n",
            env!("CARGO_PKG_VERSION")
        )
        .as_bytes(),
        0o644,
    )?;

    for name in NAMES {
        backup_destination(&bin, name)?;
        bin.symlink_replace(
            name,
            &stage("link"),
            Path::new(&format!("../libexec/bedit/current/{name}-rust")),
        )?;
    }
    install_manpages(man_root_path, &man)?;
    for (editor, wrapper) in editors {
        if mode == "transparent" {
            backup_destination(&bin, editor)?;
            let shim = format!(
                "#!/bin/sh\n# Bedit transparent editor shim\nset -eu\nBEDIT_EDITOR_ALIAS='{editor}'\nBEDIT_SHIM_DIR='{}'\nexport BEDIT_EDITOR_ALIAS BEDIT_SHIM_DIR\nexec '{}/libexec/bedit/current/{wrapper}-rust' \"$@\"\n",
                prefix_path.join("bin").display(),
                prefix_path.display(),
            );
            bin.write_replace(editor, &stage("shim"), shim.as_bytes(), 0o755)?;
        } else {
            restore_or_remove(&bin, editor)?;
        }
    }
    if let Some(config_path) = config_path {
        write_config(config_path, policy)?;
    }
    Ok(())
}

fn migrate_legacy_install(bin: &TrustedDir, libexec: &TrustedDir) -> io::Result<()> {
    for name in NAMES {
        let legacy = format!("{name}-rust");
        match libexec.stat_nofollow(&legacy) {
            Ok(stat)
                if FileType::from_raw_mode(stat.st_mode) == FileType::RegularFile
                    && stat.st_nlink == 1 =>
            {
                libexec.rename_noreplace(&legacy, &format!("{legacy}.previous"))?;
            }
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "unsafe legacy payload inode",
                ))
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    for name in ["bedit", "bed", "bvi", "bnano", "bpico", "bemacs"] {
        let stale = format!("{name}-perl");
        match bin.stat_nofollow(&stale) {
            Ok(stat) if FileType::from_raw_mode(stat.st_mode) == FileType::Symlink => {
                let target = bin.read_link(&stale)?;
                if target.ends_with(Path::new(&format!("libexec/bedit/{stale}"))) {
                    bin.unlink_file(&stale)?;
                } else {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "unsafe stale migration link",
                    ));
                }
            }
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "unsafe stale migration destination",
                ))
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        match libexec.stat_nofollow(&stale) {
            Ok(stat)
                if FileType::from_raw_mode(stat.st_mode) == FileType::RegularFile
                    && stat.st_nlink == 1 =>
            {
                libexec.unlink_file(&stale)?;
            }
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "unsafe stale migration payload",
                ))
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn validate_payload(source: &TrustedDir) -> io::Result<()> {
    let _ = source_release(source)?;
    for name in NAMES {
        let file = source.open_source_regular(&format!("{name}-rust"))?;
        if file.metadata()?.permissions().mode() & 0o111 == 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "payload is not executable",
            ));
        }
    }
    Ok(())
}

fn uninstall(
    prefix_path: &Path,
    config_path: Option<&Path>,
    editors: &[(String, String)],
    purge: bool,
) -> io::Result<()> {
    let prefix = match TrustedDir::open_absolute(prefix_path) {
        Ok(prefix) => prefix,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    let bin = optional_child(&prefix, "bin")?;
    checkpoint("uninstall-bin-opened");
    let man = optional_descendant(&prefix, &["share", "man", "man1"])?;
    let libexec_parent = optional_child(&prefix, "libexec")?;
    let libexec = match &libexec_parent {
        Some(parent) => optional_child(parent, "bedit")?,
        None => None,
    };
    let installed = match &libexec {
        Some(directory) => directory
            .read_file("install-manifest")
            .is_ok_and(|bytes| bytes.starts_with(b"mode=")),
        None => false,
    };
    if let Some(bin) = &bin {
        for name in NAMES {
            restore_or_remove(bin, name)?;
        }
        for (editor, _) in editors {
            restore_or_remove(bin, editor)?;
        }
        for editor in LEGACY_EDITORS {
            restore_or_remove(bin, editor)?;
        }
    }
    if installed {
        if let Some(man) = &man {
            for name in NAMES {
                restore_manpage(man, &format!("{name}.1"))?;
            }
        }
        if let (Some(parent), Some(_)) = (&libexec_parent, &libexec) {
            remove_tree(parent, "bedit")?;
        }
    }
    if purge {
        if let Some(config_path) = config_path {
            remove_config(config_path)?;
        }
    }
    Ok(())
}

fn source_release(source: &TrustedDir) -> io::Result<String> {
    let mut file = source.open_source_regular("bedit-rust")?;
    let mut bytes = Vec::new();
    use std::io::Read as _;
    file.read_to_end(&mut bytes)?;
    if !bytes.windows(5).any(|window| window == b"bedit") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "payload lacks Bedit marker",
        ));
    }
    Ok(format!("bedit-{}", env!("CARGO_PKG_VERSION")))
}

fn copy_payload(source: &TrustedDir, destination: &TrustedDir, name: &str) -> io::Result<()> {
    let mut input = source.open_source_regular(name)?;
    if input.metadata()?.permissions().mode() & 0o111 == 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "payload is not executable",
        ));
    }
    let mut output = destination.create_file(name, 0o755)?;
    io::copy(&mut input, &mut output)?;
    output.sync_all()?;
    fs::fchmod(&output, Mode::from_raw_mode(0o755))?;
    destination.sync()
}

fn backup_destination(directory: &TrustedDir, name: &str) -> io::Result<()> {
    match directory.stat_nofollow(name) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
        Ok(stat) => {
            let kind = FileType::from_raw_mode(stat.st_mode);
            if kind == FileType::Symlink {
                if managed_link(&directory.read_link(name)?) {
                    directory.unlink_file(name)?;
                    return Ok(());
                }
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "unsafe destination symlink",
                ));
            }
            if kind != FileType::RegularFile || stat.st_nlink != 1 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "unsafe install destination",
                ));
            }
        }
    }
    let bytes = directory.read_file(name)?;
    if is_shim(&bytes) {
        directory.unlink_file(name)
    } else {
        directory.rename_noreplace(name, &format!("{name}.bedit-backup"))
    }
}

fn restore_or_remove(directory: &TrustedDir, name: &str) -> io::Result<()> {
    match directory.stat_nofollow(name) {
        Ok(stat) if FileType::from_raw_mode(stat.st_mode) == FileType::Symlink => {
            if managed_link(&directory.read_link(name)?) {
                directory.unlink_file(name)?;
            }
        }
        Ok(stat) if FileType::from_raw_mode(stat.st_mode) == FileType::RegularFile => {
            if is_shim(&directory.read_file(name)?) {
                directory.unlink_file(name)?;
            }
        }
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unsafe uninstall destination",
            ))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    if directory
        .stat_nofollow(name)
        .is_err_and(|e| e.kind() == io::ErrorKind::NotFound)
    {
        match directory.rename_noreplace(&format!("{name}.bedit-backup"), name) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn install_manpages(source_root: &Path, destination: &TrustedDir) -> io::Result<()> {
    let source = match TrustedDir::open_absolute(&source_root.join("man/man1")) {
        Ok(source) => source,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    for name in NAMES {
        let file = format!("{name}.1");
        let bytes = match source.read_file(&file) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        match destination.read_file(&file) {
            Ok(old) if old == bytes => {}
            Ok(old) if !old.windows(5).any(|window| window == b"Bedit") => {
                destination.rename_noreplace(&file, &format!("{file}.bedit-backup"))?;
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        destination.write_replace(&file, &stage("man"), &bytes, 0o644)?;
    }
    Ok(())
}

fn restore_manpage(directory: &TrustedDir, name: &str) -> io::Result<()> {
    match directory.read_file(name) {
        Ok(bytes) if bytes.windows(5).any(|window| window == b"Bedit") => {
            directory.unlink_file(name)?;
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    if directory
        .stat_nofollow(name)
        .is_err_and(|e| e.kind() == io::ErrorKind::NotFound)
    {
        match directory.rename_noreplace(&format!("{name}.bedit-backup"), name) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn write_config(path: &Path, policy: &str) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "config has no parent"))?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid config name"))?;
    let directory = TrustedDir::open_or_create_absolute(parent, 0o755)?;
    let old = match directory.read_file(name) {
        Ok(bytes) => {
            String::from_utf8(bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error),
    };
    let mut output = String::new();
    let mut written = false;
    for line in old.lines() {
        if line.trim_start().starts_with("sudo_history") && line.contains('=') {
            if !written {
                output.push_str(&format!("sudo_history = \"{policy}\"\n"));
                written = true;
            }
        } else {
            output.push_str(line);
            output.push('\n');
        }
    }
    if !written {
        output.push_str(&format!("sudo_history = \"{policy}\"\n"));
    }
    if output.as_bytes() != old.as_bytes() {
        if !old.is_empty() {
            directory.write_replace(
                &format!("{name}.previous"),
                &stage("config-backup"),
                old.as_bytes(),
                0o644,
            )?;
        }
        directory.write_replace(name, &stage("config"), output.as_bytes(), 0o644)?;
    }
    Ok(())
}

fn remove_config(path: &Path) -> io::Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return Ok(());
    };
    let directory = match TrustedDir::open_absolute(parent) {
        Ok(directory) => directory,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    match directory.unlink_file(name) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn remove_tree(parent: &TrustedDir, name: &str) -> io::Result<()> {
    let directory = parent.child_dir(name)?;
    for entry in directory.entries()? {
        let name = entry.to_str().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "non-UTF-8 installed entry")
        })?;
        let stat = directory.stat_nofollow(name)?;
        if FileType::from_raw_mode(stat.st_mode) == FileType::Directory {
            remove_tree(&directory, name)?;
        } else {
            directory.unlink_file(name)?;
        }
    }
    parent.remove_empty_dir(name)
}

fn replace_managed_link(directory: &TrustedDir, name: &str, target: &Path) -> io::Result<()> {
    match directory.stat_nofollow(name) {
        Ok(stat) if FileType::from_raw_mode(stat.st_mode) == FileType::Symlink => {
            if !managed_current(&directory.read_link(name)?) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "unsafe managed link",
                ));
            }
        }
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unsafe managed link destination",
            ))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    directory.symlink_replace(name, &stage("current"), target)
}

fn managed_link(target: &Path) -> bool {
    target.to_string_lossy().contains("libexec/bedit/current/")
}

fn managed_current(target: &Path) -> bool {
    let text = target.to_string_lossy();
    text.starts_with("versions/") && !text.contains("..")
}

fn is_shim(bytes: &[u8]) -> bool {
    bytes
        .windows(29)
        .any(|window| window == b"Bedit transparent editor shim")
}

fn optional_child(parent: &TrustedDir, name: &str) -> io::Result<Option<TrustedDir>> {
    match parent.child_dir(name) {
        Ok(directory) => Ok(Some(directory)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn optional_descendant(root: &TrustedDir, names: &[&str]) -> io::Result<Option<TrustedDir>> {
    let mut current = match optional_child(root, names[0])? {
        Some(directory) => directory,
        None => return Ok(None),
    };
    for name in &names[1..] {
        current = match optional_child(&current, name)? {
            Some(directory) => directory,
            None => return Ok(None),
        };
    }
    Ok(Some(current))
}

fn parse_editors(value: &str) -> io::Result<Vec<(String, String)>> {
    value
        .split(',')
        .filter(|value| !value.is_empty())
        .map(|row| {
            let (editor, wrapper) = row.split_once(':').ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "invalid editor registry")
            })?;
            if !safe_name(editor) || !safe_name(wrapper) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "invalid editor name",
                ));
            }
            Ok((editor.to_owned(), wrapper.to_owned()))
        })
        .collect()
}

fn safe_name(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

fn absolute(value: &str) -> io::Result<&Path> {
    let path = Path::new(value);
    if path.is_absolute() {
        Ok(path)
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "helper path must be absolute",
        ))
    }
}

fn parse_bool(value: &str) -> io::Result<bool> {
    match value {
        "0" => Ok(false),
        "1" => Ok(true),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid helper boolean",
        )),
    }
}

fn stage(kind: &str) -> String {
    format!(".bedit-{kind}-{}-{}", std::process::id(), nonce())
}

fn nonce() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

#[cfg(not(test))]
fn checkpoint(_: &str) {}

#[cfg(test)]
type TestHook = Box<dyn FnOnce() + Send>;

#[cfg(test)]
static TEST_HOOK: std::sync::Mutex<Option<(&'static str, TestHook)>> = std::sync::Mutex::new(None);

#[cfg(test)]
fn checkpoint(name: &str) {
    let hook = {
        let mut slot = TEST_HOOK.lock().unwrap();
        if slot.as_ref().is_some_and(|(expected, _)| *expected == name) {
            slot.take().map(|(_, hook)| hook)
        } else {
            None
        }
    };
    if let Some(hook) = hook {
        hook();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};
    use std::path::PathBuf;

    static TEST_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn fixture(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("bedit-installer-{name}-{}", std::process::id()))
    }

    fn payload(base: &Path) -> PathBuf {
        let source = base.join("payload");
        std::fs::create_dir_all(&source).unwrap();
        for name in NAMES {
            let path = source.join(format!("{name}-rust"));
            std::fs::write(&path, b"#!/bin/sh\n# bedit payload\n").unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        source
    }

    fn run_install(base: &Path, source: &Path) -> io::Result<()> {
        install(
            &base.join("prefix"),
            None,
            "transparent",
            "root_and_user",
            source,
            base,
            &[("vi".into(), "bvi".into())],
            true,
        )
    }

    fn arm(expected: &'static str, hook: impl FnOnce() + Send + 'static) {
        *TEST_HOOK.lock().unwrap() = Some((expected, Box::new(hook)));
    }

    #[test]
    fn bin_swap_after_fd_acquisition_cannot_redirect_install() {
        let _serial = TEST_SERIAL.lock().unwrap();
        let base = fixture("bin-swap");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("victim")).unwrap();
        std::fs::write(base.join("victim/sentinel"), b"unchanged").unwrap();
        let source = payload(&base);
        let swap = base.clone();
        arm("installer-bin-opened", move || {
            std::fs::rename(swap.join("prefix/bin"), swap.join("detached-bin")).unwrap();
            symlink(swap.join("victim"), swap.join("prefix/bin")).unwrap();
        });
        run_install(&base, &source).unwrap();
        assert_eq!(
            std::fs::read(base.join("victim/sentinel")).unwrap(),
            b"unchanged"
        );
        assert!(base.join("detached-bin/bedit").is_symlink());
        std::fs::remove_file(base.join("prefix/bin")).unwrap();
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn libexec_swap_after_fd_acquisition_cannot_redirect_install() {
        let _serial = TEST_SERIAL.lock().unwrap();
        let base = fixture("libexec-swap");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("victim")).unwrap();
        std::fs::write(base.join("victim/sentinel"), b"unchanged").unwrap();
        let source = payload(&base);
        let swap = base.clone();
        arm("installer-libexec-opened", move || {
            std::fs::rename(swap.join("prefix/libexec"), swap.join("detached-libexec")).unwrap();
            symlink(swap.join("victim"), swap.join("prefix/libexec")).unwrap();
        });
        run_install(&base, &source).unwrap();
        assert_eq!(
            std::fs::read(base.join("victim/sentinel")).unwrap(),
            b"unchanged"
        );
        assert!(base
            .join("detached-libexec/bedit/install-manifest")
            .is_file());
        std::fs::remove_file(base.join("prefix/libexec")).unwrap();
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn uninstall_swap_stays_attached_to_opened_bin() {
        let _serial = TEST_SERIAL.lock().unwrap();
        let base = fixture("uninstall-swap");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("victim")).unwrap();
        std::fs::write(base.join("victim/sentinel"), b"unchanged").unwrap();
        let source = payload(&base);
        run_install(&base, &source).unwrap();
        let swap = base.clone();
        arm("uninstall-bin-opened", move || {
            std::fs::rename(swap.join("prefix/bin"), swap.join("detached-bin")).unwrap();
            symlink(swap.join("victim"), swap.join("prefix/bin")).unwrap();
        });
        uninstall(
            &base.join("prefix"),
            None,
            &[("vi".into(), "bvi".into())],
            false,
        )
        .unwrap();
        assert_eq!(
            std::fs::read(base.join("victim/sentinel")).unwrap(),
            b"unchanged"
        );
        assert!(!base.join("detached-bin/bedit").exists());
        std::fs::remove_file(base.join("prefix/bin")).unwrap();
        std::fs::remove_dir_all(base).unwrap();
    }
}
