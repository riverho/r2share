//! Ignore temp/hidden/partial files for folder sync.

use std::path::Path;

/// True if this path component or file name should be skipped.
pub fn is_ignored(name: &str) -> bool {
    if name.is_empty() {
        return true;
    }
    if name.starts_with('.') {
        return true;
    }
    if name.ends_with('~') {
        return true;
    }
    let lower = name.to_ascii_lowercase();
    lower.ends_with(".tmp")
        || lower.ends_with(".part")
        || lower.ends_with(".swp")
        || lower.ends_with(".swx")
        || lower == "thumbs.db"
        || lower == "desktop.ini"
}

/// True if any path component is ignored (dotdirs, temp names, etc.).
pub fn should_ignore_path(path: &Path) -> bool {
    path.components().any(|c| match c {
        std::path::Component::Normal(s) => is_ignored(&s.to_string_lossy()),
        _ => false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn ignores_dotfiles_and_temps() {
        assert!(is_ignored(".hidden"));
        assert!(is_ignored("file.tmp"));
        assert!(is_ignored("file.part"));
        assert!(is_ignored("file.swp"));
        assert!(is_ignored("file~"));
        assert!(!is_ignored("readme.md"));
        assert!(!is_ignored("photo.jpg"));
    }

    #[test]
    fn ignores_nested_dot_dirs() {
        assert!(should_ignore_path(&PathBuf::from("/a/.git/config")));
        assert!(should_ignore_path(&PathBuf::from("/a/b/file.tmp")));
        assert!(!should_ignore_path(&PathBuf::from("/a/b/ok.txt")));
    }
}
