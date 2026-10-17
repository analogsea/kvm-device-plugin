//! The guard as a real pre-push hook: Git pushes from a scratch repository to a bare remote.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;

const TERM: &str = "Forbidden-Term";

struct Fixture {
    _dir: TempDir,
    home: PathBuf,
    work: PathBuf,
}

impl Fixture {
    /// A repository with one clean commit on `main`, pushed to a bare remote, and a deny-list holding [`TERM`].
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let home = dir.path().join("home");
        let work = dir.path().join("work");
        let remote = dir.path().join("remote.git");
        fs::create_dir_all(home.join(".config/kvm-device-plugin")).unwrap();
        fs::write(
            home.join(".config/kvm-device-plugin/deny-list"),
            format!("# synthetic\n\n{TERM}\n"),
        )
        .unwrap();
        fs::write(home.join("gitconfig"), "").unwrap();
        let fixture = Self { _dir: dir, home, work };

        fixture.git(Path::new("."), &["init", "-q", "--bare", remote.to_str().unwrap()]);
        fixture.git(Path::new("."), &["init", "-q", "-b", "main", fixture.work.to_str().unwrap()]);
        fixture.run(&["remote", "add", "origin", remote.to_str().unwrap()]);
        let hook = fixture.work.join(".git/hooks/pre-push");
        fs::write(
            &hook,
            format!(
                "#!/bin/sh\nexec '{}' pre-push \"$@\"\n",
                env!("CARGO_BIN_EXE_publication-guard")
            ),
        )
        .unwrap();
        make_executable(&hook);

        fixture.commit_file("README.md", b"clean\n", "docs: start");
        fixture.push_ok(&["origin", "main"]);
        fixture
    }

    fn git(&self, dir: &Path, args: &[&str]) -> Output {
        Command::new("git")
            .current_dir(dir)
            .args(args)
            .env("HOME", &self.home)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", self.home.join("gitconfig"))
            .env("GIT_AUTHOR_NAME", "Test Author")
            .env("GIT_AUTHOR_EMAIL", "author@example.com")
            .env("GIT_AUTHOR_DATE", "2026-01-01T00:00:00Z")
            .env("GIT_COMMITTER_NAME", "Test Committer")
            .env("GIT_COMMITTER_EMAIL", "committer@example.com")
            .env("GIT_COMMITTER_DATE", "2026-01-01T00:00:00Z")
            .output()
            .unwrap()
    }

    fn run(&self, args: &[&str]) -> Output {
        let output = self.git(&self.work, args);
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    fn commit_file(&self, path: &str, content: &[u8], message: &str) {
        let file = self.work.join(path);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(file, content).unwrap();
        self.run(&["add", "--", path]);
        self.run(&["commit", "-q", "-m", message]);
    }

    fn push(&self, args: &[&str]) -> Output {
        let mut full = vec!["push", "-q"];
        full.extend_from_slice(args);
        self.git(&self.work, &full)
    }

    fn push_ok(&self, args: &[&str]) {
        let output = self.push(args);
        assert!(
            output.status.success(),
            "push {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// Asserts the push is refused and its report names `expected` without echoing the term.
    fn push_blocked(&self, args: &[&str], expected: &str) {
        let output = self.push(args);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "push {args:?} should be blocked: {stderr}");
        assert!(stderr.contains(expected), "expected {expected:?} in: {stderr}");
        assert!(
            !stderr.to_lowercase().contains(&TERM.to_lowercase()),
            "report echoes the term: {stderr}"
        );
    }
}

#[cfg(unix)]
fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn clean_push_passes() {
    let fixture = Fixture::new();
    fixture.run(&["switch", "-q", "-c", "feature"]);
    fixture.commit_file("src/lib.rs", b"pub fn clean() {}\n", "feat: add a clean change");
    fixture.push_ok(&["origin", "feature"]);
}

#[test]
fn added_line_blocks() {
    let fixture = Fixture::new();
    fixture.commit_file(
        "notes.md",
        format!("a line naming {TERM} here\n").as_bytes(),
        "docs: notes",
    );
    fixture.push_blocked(&["origin", "main"], "added content matches the deny-list's line 3");
}

#[test]
fn removed_line_passes() {
    let fixture = Fixture::new();
    // Seed the remote with a bad line without the hook, as a leak that predates it.
    fixture.commit_file("notes.md", format!("{TERM}\n").as_bytes(), "docs: notes");
    fixture.push_ok(&["--no-verify", "origin", "main"]);
    fixture.run(&["rm", "-q", "notes.md"]);
    fixture.run(&["commit", "-q", "-m", "docs: remove notes"]);
    fixture.push_ok(&["origin", "main"]);
}

#[test]
fn empty_file_with_a_bad_name_blocks() {
    let fixture = Fixture::new();
    fixture.commit_file(&format!("docs/{TERM}.md"), b"", "docs: add a file");
    fixture.push_blocked(&["origin", "main"], "file path matches");
}

#[test]
fn binary_content_blocks() {
    let fixture = Fixture::new();
    let mut content = vec![0u8, 1, 2, 0];
    content.extend_from_slice(TERM.as_bytes());
    content.push(0);
    fixture.commit_file("blob.bin", &content, "chore: add a blob");
    fixture.push_blocked(&["origin", "main"], "added content matches");
}

#[test]
fn message_blocks() {
    let fixture = Fixture::new();
    fixture.commit_file("a.txt", b"a\n", &format!("docs: mention\n\nbody names {TERM}"));
    fixture.push_blocked(&["origin", "main"], "commit message matches");
}

#[test]
fn author_email_blocks() {
    let fixture = Fixture::new();
    fs::write(fixture.work.join("a.txt"), "a\n").unwrap();
    fixture.run(&["add", "a.txt"]);
    let author = format!("Someone <someone@{TERM}.example>");
    fixture.run(&["commit", "-q", "-m", "docs: a", "--author", &author]);
    fixture.push_blocked(&["origin", "main"], "author or committer matches");
}

#[test]
fn branch_name_blocks() {
    let fixture = Fixture::new();
    let branch = format!("feat/{TERM}");
    fixture.run(&["switch", "-q", "-c", &branch]);
    fixture.push_blocked(&["origin", &branch], "ref name matches");
}

#[test]
fn annotated_tag_blocks() {
    let fixture = Fixture::new();
    fixture.run(&["tag", "-a", "v0.0.1", "-m", &format!("release for {TERM}")]);
    fixture.push_blocked(&["origin", "refs/tags/v0.0.1"], "tag matches");
}

#[test]
fn deleting_a_bad_branch_passes() {
    let fixture = Fixture::new();
    let branch = format!("old/{TERM}");
    fixture.run(&["branch", &branch]);
    fixture.push_ok(&["--no-verify", "origin", &branch]);
    fixture.push_ok(&["origin", "--delete", &branch]);
}

#[test]
fn missing_list_blocks() {
    let fixture = Fixture::new();
    fs::remove_file(fixture.home.join(".config/kvm-device-plugin/deny-list")).unwrap();
    fixture.commit_file("a.txt", b"a\n", "docs: a");
    fixture.push_blocked(&["origin", "main"], "the deny-list is missing");
}

#[test]
fn empty_list_blocks() {
    let fixture = Fixture::new();
    fs::write(fixture.home.join(".config/kvm-device-plugin/deny-list"), "# nothing\n").unwrap();
    fixture.commit_file("a.txt", b"a\n", "docs: a");
    fixture.push_blocked(&["origin", "main"], "holds no terms");
}

#[test]
fn gitleaks_blocks_a_key() {
    let fixture = Fixture::new();
    // Assembled at run time so this source never holds a key-shaped string.
    let key = ["AKIA", "QYLPMN5HHHFPZAM2"].concat();
    fixture.commit_file(
        "config.env",
        format!("aws_access_key_id = {key}\n").as_bytes(),
        "chore: config",
    );
    fixture.push_blocked(&["origin", "main"], "gitleaks found a secret");
}

#[test]
fn pushed_content_cannot_silence_gitleaks() {
    let fixture = Fixture::new();
    let key = ["AKIA", "QYLPMN5HHHFPZAM2"].concat();
    let line = format!("aws_access_key_id = {key} # gitleaks:allow\n");
    fixture.commit_file("config.env", line.as_bytes(), "chore: config");
    let key_commit = String::from_utf8(fixture.run(&["rev-parse", "HEAD"]).stdout).unwrap();
    let fingerprint = format!("{}:config.env:aws-access-token:1\n", key_commit.trim());
    let allow_all = "[extend]\nuseDefault = true\n[allowlist]\npaths = ['.*']\nregexes = ['.*']\n";
    fixture.commit_file(".gitleaks.toml", allow_all.as_bytes(), "chore: scanner config");
    fixture.commit_file(".gitleaksignore", fingerprint.as_bytes(), "chore: scanner ignores");
    fixture.push_blocked(&["origin", "main"], "gitleaks found a secret");
}

#[test]
fn stale_tracking_refs_cannot_hide_outgoing_content() {
    let fixture = Fixture::new();
    fixture.commit_file("notes.md", TERM.as_bytes(), "docs: notes");
    fixture.run(&["update-ref", "refs/remotes/origin/main", "HEAD"]);
    fixture.push_blocked(&["origin", "main"], "added content matches");
}

#[test]
fn root_content_is_scanned_even_when_git_hides_root_diffs() {
    let fixture = Fixture::new();
    fixture.run(&["switch", "--orphan", "fresh"]);
    fixture.run(&["config", "log.showRoot", "false"]);
    fixture.commit_file("notes.md", TERM.as_bytes(), "docs: start");
    fixture.push_blocked(&["origin", "fresh"], "added content matches");
}

#[test]
fn textconv_cannot_hide_private_content_or_secrets() {
    let fixture = Fixture::new();
    fixture.run(&["config", "diff.hidden.textconv", "echo clean"]);
    fixture.commit_file(".gitattributes", b"*.txt diff=hidden\n", "chore: attributes");
    let key = ["AKIA", "QYLPMN5HHHFPZAM2"].concat();
    fixture.commit_file(
        "data.txt",
        format!("{TERM}\naws_access_key_id = {key}\n").as_bytes(),
        "docs: data",
    );
    fixture.push_blocked(&["origin", "main"], "added content matches");
    fixture.push_blocked(&["origin", "main"], "gitleaks found a secret");
}

#[test]
fn scanner_diagnostics_do_not_print_private_paths_or_keys() {
    let fixture = Fixture::new();
    let key = ["AKIA", "QYLPMN5HHHFPZAM2"].concat();
    fixture.commit_file(
        &format!("{TERM}.env"),
        format!("aws_access_key_id = {key}\n").as_bytes(),
        "chore: fixture",
    );
    fixture.push_blocked(&["origin", "main"], "gitleaks found a secret");
    let output = fixture.push(&["origin", "main"]);
    assert!(!String::from_utf8_lossy(&output.stderr).contains(&key));
}

#[test]
fn noncommit_and_nested_tags_fail_closed() {
    let fixture = Fixture::new();
    let blob = String::from_utf8(fixture.run(&["rev-parse", "HEAD:README.md"]).stdout).unwrap();
    fixture.run(&["tag", "blob", blob.trim()]);
    fixture.push_blocked(&["origin", "refs/tags/blob"], "only commits and commit tags");
    fixture.run(&["tag", "-a", "inner", "-m", "inner"]);
    fixture.run(&["tag", "-a", "outer", "inner", "-m", "outer"]);
    fixture.push_blocked(&["origin", "refs/tags/outer"], "only tags directly targeting commits");
}

#[test]
fn history_scan_needs_no_private_list_and_enforces_scanner_defaults() {
    let fixture = Fixture::new();
    fs::remove_file(fixture.home.join(".config/kvm-device-plugin/deny-list")).unwrap();
    let scan = || {
        Command::new(env!("CARGO_BIN_EXE_publication-guard"))
            .arg("scan-history")
            .current_dir(&fixture.work)
            .env("HOME", &fixture.home)
            .output()
            .unwrap()
    };
    assert!(scan().status.success());
    let key = ["AKIA", "QYLPMN5HHHFPZAM2"].concat();
    fixture.commit_file(
        "config.env",
        format!("aws_access_key_id = {key} # gitleaks:allow\n").as_bytes(),
        "chore: fixture",
    );
    fixture.commit_file(
        ".gitleaks.toml",
        b"[extend]\nuseDefault = true\n[allowlist]\npaths = ['.*']\n",
        "chore: scanner config",
    );
    let output = scan();
    assert!(!output.status.success());
    assert!(!String::from_utf8_lossy(&output.stderr).contains(&key));
}

#[test]
fn scanner_execution_failure_blocks_publication() {
    let fixture = Fixture::new();
    let bin = fixture.home.join("bin");
    fs::create_dir(&bin).unwrap();
    fs::write(bin.join("gitleaks"), "#!/bin/sh\nexit 2\n").unwrap();
    make_executable(&bin.join("gitleaks"));
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let output = Command::new(env!("CARGO_BIN_EXE_publication-guard"))
        .arg("scan-history")
        .current_dir(&fixture.work)
        .env("PATH", path)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("gitleaks failed"));
}

#[test]
fn merge_resolution_binary_secrets_are_scanned_by_both_entrypoints() {
    let fixture = Fixture::new();
    fixture.run(&["switch", "-q", "-c", "side"]);
    fixture.commit_file("side.txt", b"side\n", "docs: side");
    fixture.run(&["switch", "-q", "main"]);
    fixture.commit_file("main.txt", b"main\n", "docs: main");
    fixture.run(&["merge", "--no-ff", "--no-commit", "side"]);
    let key = ["AKIA", "QYLPMN5HHHFPZAM2"].concat();
    fixture.commit_file(
        "config.bin",
        format!("\0aws_access_key_id = {key}\n").as_bytes(),
        "chore: merge",
    );
    fixture.push_blocked(&["origin", "main"], "gitleaks found a secret");
    let output = Command::new(env!("CARGO_BIN_EXE_publication-guard"))
        .arg("scan-history")
        .current_dir(&fixture.work)
        .output()
        .unwrap();
    assert!(!output.status.success());
}
