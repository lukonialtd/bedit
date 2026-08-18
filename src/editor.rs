use std::env;
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EditorFamily {
    Vim,
    Neovim,
    Nano,
    Emacs,
    Ed,
    Micro,
    Joe,
    Jed,
    McEdit,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessStrategy {
    SpawnedEditor,
    GuiOrDetachingEditor,
    ClientToExistingEditor,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EditorAlias {
    pub name: &'static str,
    pub family: EditorFamily,
    pub strategy: ProcessStrategy,
    pub named_wrapper: &'static str,
    pub supported: bool,
}

use EditorFamily::*;
use ProcessStrategy::*;

pub const EDITOR_ALIASES: &[EditorAlias] = &[
    EditorAlias {
        name: "vi",
        family: Vim,
        strategy: SpawnedEditor,
        named_wrapper: "bvi",
        supported: true,
    },
    EditorAlias {
        name: "vim",
        family: Vim,
        strategy: SpawnedEditor,
        named_wrapper: "bvi",
        supported: true,
    },
    EditorAlias {
        name: "view",
        family: Vim,
        strategy: SpawnedEditor,
        named_wrapper: "bvi",
        supported: true,
    },
    EditorAlias {
        name: "ex",
        family: Vim,
        strategy: SpawnedEditor,
        named_wrapper: "bvi",
        supported: true,
    },
    EditorAlias {
        name: "rvim",
        family: Vim,
        strategy: SpawnedEditor,
        named_wrapper: "bvi",
        supported: true,
    },
    EditorAlias {
        name: "rview",
        family: Vim,
        strategy: SpawnedEditor,
        named_wrapper: "bvi",
        supported: true,
    },
    EditorAlias {
        name: "gvim",
        family: Vim,
        strategy: GuiOrDetachingEditor,
        named_wrapper: "bvi",
        supported: false,
    },
    EditorAlias {
        name: "gview",
        family: Vim,
        strategy: GuiOrDetachingEditor,
        named_wrapper: "bvi",
        supported: false,
    },
    EditorAlias {
        name: "mvim",
        family: Vim,
        strategy: GuiOrDetachingEditor,
        named_wrapper: "bvi",
        supported: false,
    },
    EditorAlias {
        name: "nvim",
        family: Neovim,
        strategy: SpawnedEditor,
        named_wrapper: "bnvim",
        supported: true,
    },
    EditorAlias {
        name: "neovide",
        family: Neovim,
        strategy: GuiOrDetachingEditor,
        named_wrapper: "bnvim",
        supported: false,
    },
    EditorAlias {
        name: "goneovim",
        family: Neovim,
        strategy: GuiOrDetachingEditor,
        named_wrapper: "bnvim",
        supported: false,
    },
    EditorAlias {
        name: "nano",
        family: Nano,
        strategy: SpawnedEditor,
        named_wrapper: "bnano",
        supported: true,
    },
    EditorAlias {
        name: "rnano",
        family: Nano,
        strategy: SpawnedEditor,
        named_wrapper: "bnano",
        supported: true,
    },
    EditorAlias {
        name: "pico",
        family: Nano,
        strategy: SpawnedEditor,
        named_wrapper: "bpico",
        supported: true,
    },
    EditorAlias {
        name: "emacs",
        family: Emacs,
        strategy: SpawnedEditor,
        named_wrapper: "bemacs",
        supported: true,
    },
    EditorAlias {
        name: "emacs-nox",
        family: Emacs,
        strategy: SpawnedEditor,
        named_wrapper: "bemacs",
        supported: true,
    },
    EditorAlias {
        name: "xemacs",
        family: Emacs,
        strategy: SpawnedEditor,
        named_wrapper: "bemacs",
        supported: true,
    },
    EditorAlias {
        name: "emacsclient",
        family: Emacs,
        strategy: ClientToExistingEditor,
        named_wrapper: "bemacs",
        supported: false,
    },
    EditorAlias {
        name: "ed",
        family: Ed,
        strategy: SpawnedEditor,
        named_wrapper: "bed",
        supported: true,
    },
    EditorAlias {
        name: "micro",
        family: Micro,
        strategy: SpawnedEditor,
        named_wrapper: "bvi",
        supported: true,
    },
    EditorAlias {
        name: "joe",
        family: Joe,
        strategy: SpawnedEditor,
        named_wrapper: "bvi",
        supported: true,
    },
    EditorAlias {
        name: "jstar",
        family: Joe,
        strategy: SpawnedEditor,
        named_wrapper: "bvi",
        supported: true,
    },
    EditorAlias {
        name: "jed",
        family: Jed,
        strategy: SpawnedEditor,
        named_wrapper: "bvi",
        supported: true,
    },
    EditorAlias {
        name: "mcedit",
        family: McEdit,
        strategy: SpawnedEditor,
        named_wrapper: "bvi",
        supported: true,
    },
];

pub fn alias(name: &str) -> Option<&'static EditorAlias> {
    EDITOR_ALIASES.iter().find(|alias| alias.name == name)
}

pub fn resolve_executable(
    name: &str,
    path: &OsStr,
    shim_dir: Option<&Path>,
) -> io::Result<PathBuf> {
    for directory in env::split_paths(path) {
        let directory = if directory.as_os_str().is_empty() {
            PathBuf::from(".")
        } else {
            directory
        };
        if shim_dir.is_some_and(|shim| same_path(&directory, shim)) {
            continue;
        }
        let candidate = directory.join(name);
        let Ok(metadata) = fs::metadata(&candidate) else {
            continue;
        };
        if metadata.is_file()
            && metadata.permissions().mode() & 0o111 != 0
            && !is_bedit_shim(&candidate)
        {
            return Ok(candidate);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("bedit: {name} is not installed"),
    ))
}

fn is_bedit_shim(path: &Path) -> bool {
    fs::read(path).is_ok_and(|bytes| {
        let head = &bytes[..bytes.len().min(256)];
        head.windows(29)
            .any(|part| part == b"Bedit transparent editor shim")
    })
}

fn same_path(left: &Path, right: &Path) -> bool {
    match (fs::canonicalize(left), fs::canonicalize(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => left == right,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn registry_has_unique_aliases_and_explicit_strategies() {
        let mut names = HashSet::new();
        for entry in EDITOR_ALIASES {
            assert!(names.insert(entry.name), "duplicate {}", entry.name);
        }
        assert_eq!(alias("mvim").unwrap().strategy, GuiOrDetachingEditor);
        assert_eq!(
            alias("emacsclient").unwrap().strategy,
            ClientToExistingEditor
        );
        assert!(!alias("mvim").unwrap().supported);
        assert_eq!(alias("rnano").unwrap().family, Nano);
    }

    #[test]
    fn resolution_skips_shim_dir_and_handles_spaces() {
        let base = env::temp_dir().join(format!("bedit editor path {}", std::process::id()));
        let shim = base.join("shim dir");
        let real = base.join("real dir");
        fs::create_dir_all(&shim).unwrap();
        fs::create_dir_all(&real).unwrap();
        for path in [shim.join("vim"), real.join("vim")] {
            fs::write(&path, "#!/bin/sh\n").unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let path = env::join_paths([shim.as_path(), real.as_path()]).unwrap();
        assert_eq!(
            resolve_executable("vim", &path, Some(&shim)).unwrap(),
            real.join("vim")
        );
        fs::remove_dir_all(base).unwrap();
    }
}
