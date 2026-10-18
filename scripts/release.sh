#!/bin/sh
# The decisions publish.yml makes, kept here so release-test.sh can check them without GitHub or a registry.
#
#   release.sh check <tag> [dir]   Fails unless <tag> is vX.Y.Z and the workspace in [dir] (default .) is that
#                                  version, with deploy/daemonset.yaml pointing at that version's image.
#   release.sh is-latest <tag>     Prints true if <tag> is the highest vX.Y.Z tag read from standard input.
#   release.sh image-state         Reads `skopeo inspect` output on standard input, with its exit status as the
#                                  argument, and prints present or absent. Only a registry saying it doesn't know
#                                  the image or its repository means absent; any other failure is an error, so an
#                                  outage never reads as "absent" and triggers an overwrite.
set -eu

die() {
  echo "$*" >&2
  exit 1
}

# Strict, so v1.2 or v1.2.3-rc.1 never publish or take latest.
is_version_tag() {
  printf '%s\n' "$1" | grep -Eq '^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$'
}

# The highest vX.Y.Z among the tags on standard input; others are ignored.
highest() {
  while IFS= read -r tag; do
    if is_version_tag "$tag"; then printf '%s\n' "${tag#v}"; fi
  done | sort -t. -k1,1n -k2,2n -k3,3n | tail -n 1 | sed 's/^./v&/'
}

# The version in [workspace.package] of Cargo.toml.
workspace_version() {
  awk '
    /^\[/ { section = $0 }
    section == "[workspace.package]" && $1 == "version" { gsub(/"/, "", $3); print $3; exit }
  ' "$1/Cargo.toml"
}

check() {
  tag="$1"
  dir="${2:-.}"
  is_version_tag "$tag" || die "$tag is not a release tag; releases are tagged vX.Y.Z"
  version="${tag#v}"
  cargo=$(workspace_version "$dir")
  [ "$cargo" = "$version" ] || die "Cargo.toml says $cargo, but the tag is $tag"
  grep -Eq "image: ghcr\.io/analogsea/kvm-device-plugin:$version( |\$)" "$dir/deploy/daemonset.yaml" \
    || die "deploy/daemonset.yaml does not use the $version image"
}

is_latest() {
  tag="$1"
  is_version_tag "$tag" || die "$tag is not a release tag"
  [ "$(highest)" = "$tag" ] && echo true || echo false
}

image_state() {
  status="$1"
  output=$(cat)
  if [ "$status" = 0 ]; then
    echo present
  elif printf '%s\n' "$output" | grep -Eq 'manifest unknown|name unknown'; then
    echo absent
  else
    die "could not tell whether the image exists: $output"
  fi
}

command="${1:-}"
[ $# -gt 0 ] && shift
case "$command" in
  check) check "$@" ;;
  is-latest) is_latest "$@" ;;
  image-state) image_state "$@" ;;
  *) die "usage: release.sh check <tag> [dir] | is-latest <tag> | image-state <status>" ;;
esac
