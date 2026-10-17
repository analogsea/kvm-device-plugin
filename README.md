# kvm-device-plugin

A minimal Kubernetes device plugin, written in Rust, designed to let ordinary containers use the node's `/dev/kvm`
without running privileged. It speaks kubelet's device-plugin API v1beta1 over Unix sockets and does nothing else: no VM
management, no CPU or memory policy, no other devices.

## Status

| Stage | State |
| --- | --- |
| Implemented | The plugin binary: registration, allocation, health checks and recovery. |
| Tested | Unit tests, and tests against a fake kubelet over real Unix sockets, run by `mise run check` and CI. |
| Tested on real KVM | Not yet. |
| Container image, manifests, releases | Not yet. |
| Deployed | Not in any cluster. |

## How it works

The plugin advertises one extended resource, `devices.analogsea.ca/kvm` by default, as a fixed pool of slots named
`kvm-0` to `kvm-<N-1>`. Every slot stands for the same device. A container that requests one unit gets the host's
`/dev/kvm` at `/dev/kvm`, read-write:

```yaml
resources:
  limits:
    devices.analogsea.ca/kvm: 1
```

Kubernetes requires extended-resource requests, if given, to equal limits. Requesting more than one unit grants
nothing extra and uses up more slots, so request one per container.

`--slots` caps how many containers on a node can hold KVM access at once; it is not a VM count or a CPU or memory
limit. The default, 1000, is well above kubelet's default limit of 110 pods per node, so it rarely constrains
scheduling. Lower it to cap KVM users per node. Slot names depend only on the count, so they survive restarts;
raising the count keeps every existing name, and lowering it drops the highest.

### Health

Every 10 seconds, and promptly after an allocation refused because KVM was unhealthy, the plugin checks that the KVM
device is a character device, opens it read-write, checks that it reports KVM API version 12 and creates and discards a
VM (on arm64, sized to the host's address-space limit). If any step fails or takes longer than 5 seconds, every slot is
reported unhealthy, so kubelet stops placing new KVM containers on the node and the plugin refuses allocations; running
containers are not touched. Slots become healthy again when a later check passes. A plugin that starts without usable
KVM still registers, with all slots unhealthy, and recovers on its own.

The check runs with the plugin's own privileges. It proves that the node's KVM works, not that a given container may
open it: see [permissions](#permissions).

### Registration and recovery

Each time it registers, the plugin listens on a new socket in kubelet's device-plugin directory and passes kubelet a
10-second deadline. Failed registrations are retried after 1 second, doubling to at most 30 seconds, or as soon as
kubelet's socket appears or changes. A registered plugin checks every second and registers again at once when its socket
is deleted or replaced (as kubelet does when it restarts), when kubelet's socket changes, or when nothing has watched
its devices for 5 seconds (kubelet never reconnects on its own). Before each registration, it deletes sockets that
earlier instances of the same resource left behind, but leaves live ones alone, so a new instance can start before the
old one stops. SIGTERM or SIGINT ends open device streams, removes the plugin's socket and exits; a stuck call or KVM
check delays the exit by at most about 10 seconds.

### Permissions

Allocation asks the container runtime to give the container access to `/dev/kvm` in its device cgroup. The device node's
owner, group and mode still apply inside the container. A container running as root can open it. systemd's default makes
`/dev/kvm` mode `0666` with group `kvm`, so any user can open it; some distributions use `0660` instead, and then a
non-root container needs the device's group, whose ID varies by distribution, for example through `supplementalGroups`.
Consumers should not need privileged mode; this is not yet verified on a real cluster.

## Usage

```text
kvm-device-plugin [--resource-name DOMAIN/NAME] [--slots COUNT] [--device-plugin-dir DIR] [--kvm-path PATH]
```

| Option | Default | Meaning |
| --- | --- | --- |
| `--resource-name` | `devices.analogsea.ca/kvm` | Extended resource name, validated as kubelet does. |
| `--slots` | `1000` | Containers that can hold the resource at once, 1 to 10000. |
| `--device-plugin-dir` | `/var/lib/kubelet/device-plugins` | Kubelet's device-plugin directory. |
| `--kvm-path` | `/dev/kvm` | The device the plugin checks. Containers always receive the host's `/dev/kvm`. |

The plugin logs at INFO level to standard error; the level is not configurable. It exits with status 0 after SIGTERM or
SIGINT, 2 for invalid options and 1 if it cannot run.

### Known limitations

- It runs on Linux on amd64 and arm64. Other builds, used only for development, always report KVM as unhealthy.
- Only one device, the host's `/dev/kvm`, can be shared. Topology hints and preferred allocations are not offered.
- The plugin itself must be able to open the KVM device to check it, which likely requires a privileged container or
  equivalent device access for the plugin, though not for its consumers. Its minimum privileges are not yet
  established.
- Any client that watches the plugin's devices counts as kubelet for detecting a lost connection.
- The crate's library exists for the binary and its tests and is not a stable API.

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
It lints the plugin for both Linux release targets. Tests that open real device files run only on Linux.

## Attribution

The publication guard and its tests are adapted from [acyclic](https://github.com/iExalt/acyclic), revision
`9c95cb57bdcd4df002fc7eb426a3f05dacdb6136`, under the MIT license. The existing MIT license preserves its copyright
notice. `proto/deviceplugin/v1beta1/api.proto` is copied unmodified from Kubernetes v1.37.1
(`staging/src/k8s.io/kubelet/pkg/apis/deviceplugin/v1beta1/api.proto`) under the Apache License 2.0, included as
`proto/LICENSE`. Its generated bindings are in `crates/kvm-device-plugin/src/proto/v1beta1.rs`; regenerate them with
`mise run proto:gen`, which CI checks with `mise run proto:check`. The `.gitignore` is GitHub's unmodified
[Rust template](https://github.com/github/gitignore/blob/main/Rust.gitignore).
