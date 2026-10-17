//! The ref updates Git passes to a pre-push hook on standard input.

use std::fmt;

/// One line of a pre-push hook's input: `<local ref> <local sha> <remote ref> <remote sha>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefUpdate {
    /// The local ref being pushed, or `(delete)`.
    pub local_ref: String,
    /// The object being pushed; all zeros for a deletion.
    pub local_sha: String,
    /// The ref on the remote.
    pub remote_ref: String,
    /// The remote ref's current object; all zeros when the ref is new.
    pub remote_sha: String,
}

impl RefUpdate {
    /// Whether the update deletes the remote ref.
    #[must_use]
    pub fn is_deletion(&self) -> bool {
        is_zero(&self.local_sha)
    }

    /// Whether the remote ref doesn't exist yet.
    #[must_use]
    pub fn is_new(&self) -> bool {
        is_zero(&self.remote_sha)
    }
}

/// A malformed input line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    /// The 1-based line number.
    pub line: usize,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "malformed pre-push input on line {}", self.line)
    }
}

impl std::error::Error for ParseError {}

fn is_zero(sha: &str) -> bool {
    sha.bytes().all(|byte| byte == b'0')
}

fn is_sha(field: &str) -> bool {
    matches!(field.len(), 40 | 64) && field.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Parses a pre-push hook's input, skipping blank lines.
///
/// # Errors
///
/// Fails on a line without four fields or with a malformed object id.
pub fn parse_updates(input: &str) -> Result<Vec<RefUpdate>, ParseError> {
    input
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(index, line)| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            match fields[..] {
                [local_ref, local_sha, remote_ref, remote_sha]
                    if is_sha(local_sha) && is_sha(remote_sha) =>
                {
                    Ok(RefUpdate {
                        local_ref: local_ref.to_owned(),
                        local_sha: local_sha.to_owned(),
                        remote_ref: remote_ref.to_owned(),
                        remote_sha: remote_sha.to_owned(),
                    })
                },
                _ => Err(ParseError { line: index + 1 }),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "1111111111111111111111111111111111111111";
    const Z: &str = "0000000000000000000000000000000000000000";

    #[test]
    fn parses_updates_and_classifies_them() {
        let input = format!(
            "refs/heads/main {A} refs/heads/main {A}\n\n(delete) {Z} refs/heads/old {A}\nrefs/heads/new {A} refs/heads/new {Z}\n"
        );
        let updates = parse_updates(&input).unwrap();
        assert_eq!(updates.len(), 3);
        assert!(!updates[0].is_deletion() && !updates[0].is_new());
        assert!(updates[1].is_deletion());
        assert!(updates[2].is_new() && !updates[2].is_deletion());
        assert_eq!(updates[2].remote_ref, "refs/heads/new");
    }

    #[test]
    fn rejects_malformed_lines() {
        assert_eq!(parse_updates("refs/heads/main abc"), Err(ParseError { line: 1 }));
        let bad_sha = format!("\nrefs/heads/main xyz refs/heads/main {A}");
        assert_eq!(parse_updates(&bad_sha), Err(ParseError { line: 2 }));
    }

    #[test]
    fn empty_input_is_no_updates() {
        assert_eq!(parse_updates(""), Ok(vec![]));
    }
}
