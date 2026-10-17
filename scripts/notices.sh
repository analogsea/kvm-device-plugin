#!/bin/sh
# Writes the release's notices to the path given: THIRD_PARTY.md, the licences of the Rust crates the plugin links on
# every release target, the notices of musl and Rust's standard library, which the static binaries embed, then
# Kubernetes's licence for the vendored proto. The Apache License 2.0 text there also covers Rust's standard library.
set -eu
root="$(cd "$(dirname "$0")/.." && pwd)"
out="$1"
crates=$(mktemp)
trap 'rm -f "$crates"' EXIT
cargo about generate --locked -c "$root/scripts/notices/about.toml" \
  -m "$root/crates/kvm-device-plugin/Cargo.toml" "$root/scripts/notices/crates.hbs" -o "$crates"
{
  cat "$root/THIRD_PARTY.md"
  echo
  cat "$crates"
  echo
  echo "## musl 1.2.5"
  echo
  echo '```text'
  cat "$root/scripts/notices/musl-COPYRIGHT"
  echo '```'
  echo
  echo "## The Rust project's standard library"
  echo
  echo '```text'
  cat "$root/scripts/notices/rust-LICENSE-MIT"
  echo '```'
  echo
  echo "## Kubernetes, and the Rust project under the Apache License 2.0"
  echo
  echo '```text'
  cat "$root/proto/LICENSE"
  echo '```'
} > "$out"
