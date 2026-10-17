# kvm-device-plugin

A minimal Rust Kubernetes device plugin for `/dev/kvm`, under development.
The repository currently contains publication safeguards; the runtime plugin is not implemented yet.

## Contributing

Install [mise](https://mise.jdx.dev/), then run:

```sh
mise trust
mise install
mise run hooks:install
mise run check
```

Before pushing, create a nonempty private deny-list at `~/.config/kvm-device-plugin/deny-list`.
Use one private term per line; blank lines and `#` comments are ignored. Keep this file outside Git and CI.
A missing, unreadable or empty list blocks pushes. The hook also requires Git and the pinned Gitleaks scanner.

The [publication guard](tools/publication-guard/README.md) checks outgoing history and ref names before a push.
Never bypass it. Synthetic tests use temporary local repositories and never push fixtures to GitHub.
Hooks do not protect web edits, discussions, other clones or CI-generated pushes; repository policies and GitHub
secret scanning and push protection provide complementary coverage. Secret scanning cannot identify every private
term or credential, so review all public content before sending it.

CI runs the routine checks and `mise run check:secrets`, which scans all fetched history with enforced Gitleaks defaults
without loading the private deny-list. Workflows use read-only permissions and do not receive private configuration.

The routine check has a 60-second warm-cache budget. Cold tool installation and compilation are measured separately.

## Attribution

The publication guard and its tests are adapted from [acyclic](https://github.com/iExalt/acyclic), revision
`9c95cb57bdcd4df002fc7eb426a3f05dacdb6136`, under the MIT license. The existing MIT license preserves its copyright
notice. The `.gitignore` is GitHub's unmodified
[Rust template](https://github.com/github/gitignore/blob/main/Rust.gitignore).
