#!/bin/sh
# Tests release.sh's decisions against synthetic fixtures. Run by `mise run check`.
set -eu
release="$(dirname "$0")/release.sh"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
failures=0

expect() {
  want="$1"
  shift
  if got=$("$@" 2>/dev/null); then :; else got="fails"; fi
  if [ "$got" != "$want" ]; then
    echo "FAIL: $* gave '$got', want '$want'" >&2
    failures=$((failures + 1))
  fi
}

# A workspace at version $1 whose DaemonSet uses image $2.
workspace() {
  dir="$work/$1-$2"
  mkdir -p "$dir/deploy"
  printf '[workspace]\nmembers = []\n\n[workspace.package]\nversion = "%s"\nedition = "2024"\n\n[dependencies]\nversion = "9.9.9"\n' \
    "$1" > "$dir/Cargo.toml"
  printf '          image: ghcr.io/analogsea/kvm-device-plugin:%s # x-release-please-version\n' "$2" \
    > "$dir/deploy/daemonset.yaml"
  echo "$dir"
}

ok() { "$@" >/dev/null && echo ok; }

# Tags.
expect ok ok sh "$release" check v1.2.3 "$(workspace 1.2.3 1.2.3)"
for tag in 1.2.3 v1.2 v1.2.3-rc.1 v01.2.3 v1.2.3.4 'v1.2.3 '; do
  expect fails ok sh "$release" check "$tag" "$(workspace 1.2.3 1.2.3)"
done

# The versions a release-please updater may have missed.
expect fails ok sh "$release" check v1.2.3 "$(workspace 1.2.2 1.2.3)"
expect fails ok sh "$release" check v1.2.3 "$(workspace 1.2.3 1.2.2)"
expect fails ok sh "$release" check v1.2.3 "$(workspace 1.2.3 1.2.30)"

# latest goes only to the highest tag, even when an older one publishes last.
tags='v0.1.0
v0.2.0
v0.1.1
v0.10.0-rc.1
not-a-version'
expect false sh -c "printf '%s\n' '$tags' | sh '$release' is-latest v0.1.1"
expect true sh -c "printf '%s\n' '$tags' | sh '$release' is-latest v0.2.0"
expect true sh -c "printf 'v0.9.0\nv0.10.0\n' | sh '$release' is-latest v0.10.0"
expect false sh -c "printf 'v0.9.0\nv0.10.0\n' | sh '$release' is-latest v0.9.0"
expect fails sh -c "printf 'v1.0.0-rc.1\n' | sh '$release' is-latest v1.0.0-rc.1"

# Write-once: only a registry that says it does not know the image lets publishing push.
expect present sh -c "echo '{}' | sh '$release' image-state 0"
expect absent sh -c "echo 'reading manifest 1.2.3 in ghcr.io/x/y: manifest unknown' | sh '$release' image-state 1"
expect absent sh -c "echo 'name unknown: repository name not known to registry' | sh '$release' image-state 1"
expect fails sh -c "echo 'unauthorized: authentication required' | sh '$release' image-state 1"

[ "$failures" = 0 ] || exit 1
