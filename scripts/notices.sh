#!/bin/sh
# Writes the release's notices to the path given: THIRD_PARTY.md, the licences of the Rust crates the plugin links on
# every release target, then Kubernetes's licence for the vendored proto.
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
  echo "## Kubernetes"
  echo
  echo '```text'
  cat "$root/proto/LICENSE"
  echo '```'
} > "$out"
