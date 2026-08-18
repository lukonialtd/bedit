use std::path::{Path, PathBuf};

pub const SEPARATOR: &str = "::_::";

pub fn directory_key(directory: &Path) -> String {
    let text = directory.to_string_lossy();
    let without_root = text.strip_prefix('/').unwrap_or(&text);
    let mapped: String = without_root
        .chars()
        .map(|c| {
            if c == '/' {
                '_'
            } else if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if mapped.is_empty() {
        "root".to_owned()
    } else {
        mapped
    }
}

pub fn record_name(key: &str, leaf: &str, revision: u64, kind: char) -> String {
    format!("{key}{SEPARATOR}{leaf}{SEPARATOR}{revision}{SEPARATOR}{kind}")
}

pub fn parse_record_name(name: &str, expected_kind: char) -> Option<(String, String, u64)> {
    let mut parts = name.split(SEPARATOR);
    let key = parts.next()?;
    let leaf = parts.next()?;
    let revision = parts.next()?.parse().ok()?;
    let kind = parts.next()?;
    if parts.next().is_some() || kind != expected_kind.to_string() {
        return None;
    }
    Some((key.to_owned(), leaf.to_owned(), revision))
}

pub fn record_path(root: &Path, key: &str, leaf: &str, revision: u64, kind: char) -> PathBuf {
    root.join(record_name(key, leaf, revision, kind))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_legacy_directory_key_encoding() {
        assert_eq!(
            directory_key(Path::new("/tmp/a b/example")),
            "tmp_a_b_example"
        );
        assert_eq!(directory_key(Path::new("/")), "root");
    }

    #[test]
    fn parses_legacy_record_name() {
        let parsed = parse_record_name("tmp_demo::_::a.txt::_::12::_::b", 'b').unwrap();
        assert_eq!(parsed, ("tmp_demo".into(), "a.txt".into(), 12));
    }
}
