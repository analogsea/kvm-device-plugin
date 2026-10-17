//! Loading the deny-list and matching text against it.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

/// The deny-list's location under a home directory.
#[must_use]
pub fn path_under(home: &Path) -> PathBuf {
    home.join(".config").join("kvm-device-plugin").join("deny-list")
}

/// A parsed deny-list: fixed strings, matched case-insensitively.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DenyList {
    entries: Vec<Entry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    line: usize,
    needle: String,
}

/// Why the deny-list couldn't be loaded. Every case blocks the push.
#[derive(Debug)]
pub enum LoadError {
    /// `HOME` is unset, so the list can't be located.
    NoHome,
    /// No file at the list's path.
    Missing(PathBuf),
    /// The file exists but couldn't be read.
    Unreadable(PathBuf, io::Error),
    /// The file holds no terms.
    Empty(PathBuf),
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoHome => write!(f, "HOME is unset, so the deny-list can't be located"),
            Self::Missing(path) => write!(f, "the deny-list is missing: {}", path.display()),
            Self::Unreadable(path, err) => {
                write!(f, "the deny-list can't be read: {}: {err}", path.display())
            },
            Self::Empty(path) => write!(f, "the deny-list holds no terms: {}", path.display()),
        }
    }
}

impl std::error::Error for LoadError {}

impl DenyList {
    /// Parses the list: one term per line, trimmed; blank lines and `#` comments skipped.
    #[must_use]
    pub fn parse(text: &str) -> Self {
        let entries = text
            .lines()
            .enumerate()
            .filter_map(|(index, line)| {
                let term = line.trim();
                (!term.is_empty() && !term.starts_with('#'))
                    .then(|| Entry { line: index + 1, needle: term.to_lowercase() })
            })
            .collect();
        Self { entries }
    }

    /// Loads the list from `$HOME/.config/kvm-device-plugin/deny-list`.
    ///
    /// # Errors
    ///
    /// Fails when `HOME` is unset or the list is missing, unreadable or empty.
    pub fn load_from_home() -> Result<Self, LoadError> {
        let home = std::env::var_os("HOME").filter(|home| !home.is_empty());
        let home = home.ok_or(LoadError::NoHome)?;
        Self::load(&path_under(Path::new(&home)))
    }

    /// Loads the list from a path.
    ///
    /// # Errors
    ///
    /// Fails when the list is missing, unreadable or empty.
    pub fn load(path: &Path) -> Result<Self, LoadError> {
        let text = std::fs::read_to_string(path).map_err(|err| match err.kind() {
            io::ErrorKind::NotFound => LoadError::Missing(path.to_owned()),
            _ => LoadError::Unreadable(path.to_owned(), err),
        })?;
        let list = Self::parse(&text);
        if list.entries.is_empty() {
            return Err(LoadError::Empty(path.to_owned()));
        }
        Ok(list)
    }

    /// The number of terms.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the list holds no terms.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The list's line numbers of every term found in `text`, in list order.
    #[must_use]
    pub fn matches(&self, text: &str) -> Vec<usize> {
        let text = text.to_lowercase();
        self.entries
            .iter()
            .filter(|entry| text.contains(&entry.needle))
            .map(|entry| entry.line)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_skips_blanks_and_comments_and_keeps_line_numbers() {
        let list = DenyList::parse("# comment\n\n  alpha  \nbeta gamma\n   # indented comment\n");
        assert_eq!(list.len(), 2);
        assert_eq!(list.matches("xx ALPHA yy"), vec![3]);
        assert_eq!(list.matches("Beta Gamma"), vec![4]);
    }

    #[test]
    fn matches_are_fixed_case_insensitive_substrings() {
        let list = DenyList::parse("a.c\nsecret[1]\n");
        assert!(list.matches("abc").is_empty(), "terms are not regular expressions");
        assert_eq!(list.matches("xA.Cx"), vec![1]);
        assert_eq!(list.matches("my-SECRET[1]-value"), vec![2]);
        assert_eq!(list.matches("a.c secret[1]"), vec![1, 2]);
        assert_eq!(list.matches("nothing here"), Vec::<usize>::new());
    }

    #[test]
    fn load_fails_closed() {
        let dir =
            std::env::temp_dir().join(format!("publication-guard-unit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("deny-list");

        assert!(matches!(DenyList::load(&path), Err(LoadError::Missing(_))));
        std::fs::write(&path, "# only a comment\n\n").unwrap();
        assert!(matches!(DenyList::load(&path), Err(LoadError::Empty(_))));
        std::fs::write(&path, [0xff, 0xfe, b'\n']).unwrap();
        assert!(matches!(DenyList::load(&path), Err(LoadError::Unreadable(..))));
        std::fs::write(&path, "term\n").unwrap();
        assert_eq!(DenyList::load(&path).unwrap().len(), 1);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn path_is_under_dot_config() {
        assert_eq!(
            path_under(Path::new("/h")),
            Path::new("/h/.config/kvm-device-plugin/deny-list")
        );
    }
}
