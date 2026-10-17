//! `publication-guard pre-push <remote> <url>`: the pre-push hook. It reads Git's ref updates on standard input.

use std::io::Read;
use std::path::Path;
use std::process::ExitCode;

use publication_guard::{DenyList, push, scan};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [command, remote, _url] = args.as_slice() else {
        eprintln!("usage: publication-guard pre-push <remote> <url>");
        return ExitCode::from(2);
    };
    if command != "pre-push" {
        eprintln!("usage: publication-guard pre-push <remote> <url>");
        return ExitCode::from(2);
    }
    match run(remote) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => {
            eprintln!("publication-guard: push blocked. Fix the content; never bypass this check.");
            ExitCode::FAILURE
        },
        Err(err) => {
            eprintln!("publication-guard: push blocked: {err}");
            ExitCode::from(2)
        },
    }
}

fn run(remote: &str) -> Result<bool, Box<dyn std::error::Error>> {
    let list = DenyList::load_from_home()?;
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)?;
    let updates = push::parse_updates(&input)?;
    let report = scan::scan_push(Path::new("."), remote, &updates, &list)?;
    for finding in &report.findings {
        eprintln!("publication-guard: {finding}");
    }
    if report.leaks {
        eprintln!("publication-guard: gitleaks found a secret");
    }
    Ok(report.is_clean())
}
