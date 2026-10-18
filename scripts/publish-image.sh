#!/bin/sh
# Pushes the plugin's per-architecture OCI archives, image-amd64.tar and image-arm64.tar in <dir>, to <repository> as
# <tag>-<arch>, then makes <tag> one index of both, linked to this repository. With --latest, the index is also tagged
# `latest`. Extra arguments go to skopeo, such as --dest-tls-verify=false for a local registry.
set -eu
repository="$1"
tag="$2"
dir="$3"
shift 3
latest=""
if [ "${1:-}" = --latest ]; then
  latest="--tag $repository:latest"
  shift
fi
for arch in amd64 arm64; do
  skopeo copy --all "$@" "oci-archive:$dir/image-$arch.tar" "docker://$repository:$tag-$arch"
done
# shellcheck disable=SC2086 # $latest is either empty or two words.
docker buildx imagetools create \
  --annotation "index:org.opencontainers.image.source=https://github.com/analogsea/kvm-device-plugin" \
  --tag "$repository:$tag" $latest \
  "$repository:$tag-amd64" "$repository:$tag-arm64"
