# The plugin's image: the static binary and its notices, and nothing else, not even a shell. It runs as root, which
# kubelet's device-plugin directory requires. Built one architecture per native runner by .github/workflows/build.yml,
# from a release binary staged in dist/<arch>/ and the notices in dist/.
FROM scratch

ARG TARGETARCH

COPY LICENSE dist/THIRD_PARTY_NOTICES.md /usr/share/kvm-device-plugin/
COPY dist/${TARGETARCH}/kvm-device-plugin /usr/local/bin/kvm-device-plugin

LABEL org.opencontainers.image.source="https://github.com/analogsea/kvm-device-plugin" \
      org.opencontainers.image.description="A Kubernetes device plugin that exposes /dev/kvm to containers" \
      org.opencontainers.image.licenses="MIT"

ENTRYPOINT ["/usr/local/bin/kvm-device-plugin"]
