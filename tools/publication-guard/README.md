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
