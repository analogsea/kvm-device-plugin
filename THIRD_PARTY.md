# Third-party code

## Kubernetes device-plugin API

`proto/deviceplugin/v1beta1/api.proto` is copied unmodified from Kubernetes v1.37.1, and the plugin's API bindings
are generated from it. Kubernetes is under the Apache License 2.0, whose text follows the crates' licences below.

- Source: <https://github.com/kubernetes/kubernetes>, `staging/src/k8s.io/kubelet/pkg/apis/deviceplugin/v1beta1/`.

## Release binaries

The Linux binaries are linked statically against musl, under the MIT License (<https://musl.libc.org>).
