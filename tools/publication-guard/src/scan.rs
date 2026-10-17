//! Scanning a push's ref updates with Git and Gitleaks.

use std::ffi::OsStr;
use std::fmt;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use crate::{DenyList, RefUpdate};

/// Where a deny-listed term was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    /// The local or remote ref name.
    RefName,
    /// A commit's author or committer name or email.
    Identity,
    /// A commit's message.
    Message,
    /// The path of an added, modified or type-changed file.
    Path,
    /// An added line of a commit's patch.
    Content,
    /// An annotated tag's text, including its tagger.
    Tag,
}

impl fmt::Display for Channel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::RefName => "ref name",
            Self::Identity => "author or committer",
            Self::Message => "commit message",
            Self::Path => "file path",
            Self::Content => "added content",
            Self::Tag => "tag",
        })
    }
}

/// A deny-listed term found in an outgoing object or ref name. It never holds the term.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// The commit or tag object, or which side's ref for [`Channel::RefName`], whose name may hold the term.
    pub object: String,
    /// Where the term was found.
    pub channel: Channel,
    /// The deny-list line holding the term.
    pub entry_line: usize,
}

impl fmt::Display for Finding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}: {} matches the deny-list's line {}",
            self.object, self.channel, self.entry_line
        )
    }
}

/// A failure to run Git or Gitleaks. It blocks the push.
#[derive(Debug)]
pub struct Error(String);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

/// What scanning a push found.
#[derive(Debug, Default)]
pub struct Report {
    /// Deny-list matches.
    pub findings: Vec<Finding>,
    /// Whether Gitleaks reported a leak.
    pub leaks: bool,
}

impl Report {
    /// Whether the push may proceed.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.findings.is_empty() && !self.leaks
    }
}

/// Scans every update of a push to `remote` from the repository at `repo`.
///
/// # Errors
///
/// Fails when Git or Gitleaks can't run or Gitleaks fails for a reason other than a leak.
pub fn scan_push(
    repo: &Path,
    _remote: &str,
    updates: &[RefUpdate],
    list: &DenyList,
) -> Result<Report, Error> {
    let mut report = Report::default();
    for update in updates.iter().filter(|update| !update.is_deletion()) {
        let names = [("the local ref", &update.local_ref), ("the remote ref", &update.remote_ref)];
        for (side, name) in names {
            report.findings.extend(find(list, name, Channel::RefName, side));
        }
        let object_type = git_text(repo, ["cat-file", "-t", &update.local_sha])?;
        match object_type.trim() {
            "commit" => {},
            "tag" => {
                let text = git_text(repo, ["cat-file", "-p", &update.local_sha])?;
                if !text
                    .lines()
                    .take_while(|line| !line.is_empty())
                    .any(|line| line == "type commit")
                {
                    return Err(Error("only tags directly targeting commits can be pushed".into()));
                }
                report.findings.extend(find(list, &text, Channel::Tag, &update.local_sha));
            },
            _ => return Err(Error("only commits and commit tags can be pushed".into())),
        }
        let range = outgoing_range(repo, update)?;
        let commits =
            git_text(repo, ["rev-list"].into_iter().chain(range.iter().map(String::as_str)))?;
        let commits: Vec<&str> = commits.lines().collect();
        for commit in &commits {
            scan_commit(repo, list, commit, &mut report.findings)?;
        }
        if !commits.is_empty() {
            report.leaks |= gitleaks(repo, &range)?;
        }
    }
    Ok(report)
}

/// Scans all reachable Git history for secrets, without loading private configuration.
///
/// # Errors
///
/// Fails closed if Git or Gitleaks cannot complete the scan.
pub fn scan_history(repo: &Path) -> Result<bool, Error> {
    gitleaks(repo, &["--all".to_owned()])
}

/// The `rev-list` arguments selecting commits the remote doesn't have yet.
fn outgoing_range(repo: &Path, update: &RefUpdate) -> Result<Vec<String>, Error> {
    // Trust only the destination's advertised object, never local tracking refs.
    let mut range = vec![update.local_sha.clone()];
    if !update.is_new() && has_commit(repo, &update.remote_sha)? {
        range.push(format!("^{}", update.remote_sha));
    }
    Ok(range)
}

fn has_commit(repo: &Path, sha: &str) -> Result<bool, Error> {
    let output = git(repo, ["cat-file", "-e", &format!("{sha}^{{commit}}")])?;
    Ok(output.status.success())
}

fn scan_commit(
    repo: &Path,
    list: &DenyList,
    commit: &str,
    findings: &mut Vec<Finding>,
) -> Result<(), Error> {
    let header = git_text(repo, ["show", "-s", "--format=%an%x00%ae%x00%cn%x00%ce%x00%B", commit])?;
    let mut fields = header.splitn(5, '\0');
    let identity: Vec<&str> = fields.by_ref().take(4).collect();
    findings.extend(find(list, &identity.join("\n"), Channel::Identity, commit));
    findings.extend(find(list, fields.next().unwrap_or_default(), Channel::Message, commit));

    let paths = git_text(
        repo,
        [
            "diff-tree",
            "-r",
            "-z",
            "--root",
            "--no-commit-id",
            "--no-renames",
            "--name-status",
            "--diff-merges=first-parent",
            commit,
        ],
    )?;
    let mut entries = paths.split('\0');
    while let (Some(status), Some(path)) = (entries.next(), entries.next()) {
        if matches!(status, "A" | "M" | "T") {
            findings.extend(find(list, path, Channel::Path, commit));
        }
    }

    let patch = git_text(
        repo,
        [
            "log",
            "-1",
            "-p",
            "--text",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "--root",
            "--no-renames",
            "--diff-merges=first-parent",
            "--format=",
            commit,
        ],
    )?;
    let added: String =
        patch.lines().filter(|line| line.starts_with('+')).flat_map(|line| [line, "\n"]).collect();
    findings.extend(find(list, &added, Channel::Content, commit));
    Ok(())
}

fn find(list: &DenyList, text: &str, channel: Channel, object: &str) -> Vec<Finding> {
    list.matches(text)
        .into_iter()
        .map(|entry_line| Finding { object: object.to_owned(), channel, entry_line })
        .collect()
}

/// Gitleaks's exit code for a leak. Its own errors exit with 1, so a leak needs a distinct code.
const GITLEAKS_LEAK_EXIT: i32 = 10;

/// Gitleaks's built-in rules, passed explicitly so that a `.gitleaks.toml` in the pushed tree can't replace them.
const GITLEAKS_CONFIG: &str = "[extend]\nuseDefault = true\n";

/// Runs Gitleaks over the outgoing commits; `true` when it found a leak.
///
/// Nothing in the pushed content can weaken the scan: the configuration is ours, `gitleaks:allow` comments are
/// ignored, and no ignore file in the work tree or the pushed content is read.
fn gitleaks(repo: &Path, range: &[String]) -> Result<bool, Error> {
    // Gitleaks reads `.gitleaksignore` from the directory it scans, so it scans the Git directory, not the work tree.
    let git_dir = git_text(repo, ["rev-parse", "--absolute-git-dir"])?;
    let dir = PrivateDir::new()?;
    let config = dir.0.join("gitleaks.toml");
    std::fs::write(&config, GITLEAKS_CONFIG)
        .map_err(|err| Error(format!("can't write gitleaks' configuration: {err}")))?;
    let output = Command::new("gitleaks")
        .args(["git", "--no-banner", "--redact", "--ignore-gitleaks-allow", "--exit-code"])
        .arg(GITLEAKS_LEAK_EXIT.to_string())
        .arg("--config")
        .arg(&config)
        .arg("--gitleaks-ignore-path")
        .arg(&dir.0)
        .arg("--log-opts")
        .arg(format!(
            "--root --no-textconv --no-ext-diff --diff-merges=first-parent --text {}",
            range.join(" ")
        ))
        .arg(git_dir.trim())
        .env_remove("GITLEAKS_CONFIG")
        .env_remove("GITLEAKS_CONFIG_TOML")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|err| Error(format!("gitleaks can't run: {err}")))?;
    match output.status.code() {
        Some(0) => Ok(false),
        Some(GITLEAKS_LEAK_EXIT) => Ok(true),
        _ => Err(Error(format!("gitleaks failed: {}", output.status))),
    }
}

/// A fresh, empty directory, removed on drop.
struct PrivateDir(std::path::PathBuf);

impl PrivateDir {
    fn new() -> Result<Self, Error> {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos());
        let path =
            std::env::temp_dir().join(format!("publication-guard-{}-{nanos}", std::process::id()));
        std::fs::create_dir(&path)
            .map_err(|err| Error(format!("can't create {}: {err}", path.display())))?;
        Ok(Self(path))
    }
}

impl Drop for PrivateDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn git<I, S>(repo: &Path, args: I) -> Result<Output, Error>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["-c", "core.quotePath=false"])
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|err| Error(format!("git can't run: {err}")))
}

fn git_text<I, S>(repo: &Path, args: I) -> Result<String, Error>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let output = git(repo, args)?;
    if !output.status.success() {
        return Err(Error(format!("git failed ({})", output.status)));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}
