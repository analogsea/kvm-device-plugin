# `publication-guard`

The publication guard for the public repository. Before a push leaves the machine, it checks every outgoing commit
against a deny-list of private terms and a secret scanner, and blocks the push on any match or error.

## The deny-list

The list lives at `~/.config/kvm-device-plugin/deny-list`, outside every repository, and is never published. Each
non-blank line is one term; lines starting with `#` are comments. Terms are fixed strings, matched case-insensitively
anywhere in the scanned text. A missing, unreadable or empty list blocks every push.

## What a push is checked for

For each pushed ref (deletions excepted), the guard scans the local and remote ref names and every commit not reachable
from the destination's advertised old ref:

- author and committer names and emails, and the full message;
- the paths of added, modified and type-changed files;
- every added line of the patch, binary files included, with merges compared against their first parent;
- the text of annotated tags.

Reports name the commit, the channel and the deny-list line that matched, never the term itself. Gitleaks then scans the
same commits with its default rules, ignoring any `.gitleaks.toml`, `.gitleaksignore` or `gitleaks:allow` in the pushed
content; a leak or a Gitleaks error blocks the push. New refs scan their entire reachable history. Local tracking refs
cannot suppress a scan. Only commits and annotated tags directly targeting commits are supported; other object types
block the push. Git text conversion and root-diff suppression cannot hide content. Scanner output is captured so private
paths and metadata cannot leak into diagnostics.

Install the hook with `mise run hooks:install`. Never bypass it; if it blocks, fix the content.

## CI history scanning

`publication-guard scan-history` scans all reachable Git history with the same enforced Gitleaks configuration.
It does not load the private deny-list. CI must fetch full history; a shallow checkout only scans the history it has.
Gitleaks output is captured, and the command reports only success or failure. This does not replace the local
private-term check, and it cannot prevent a secret from first reaching GitHub; push protection is a separate layer.

## Detection limits

The private-term matcher searches binary additions as text too. Gitleaks uses its built-in secret rules and may skip
binary content or miss encoded or unknown credential formats; `--text` does not remove those scanner limitations.
Review binary artifacts and public content manually. Neither scanner guarantees that a clean result is safe to publish.
