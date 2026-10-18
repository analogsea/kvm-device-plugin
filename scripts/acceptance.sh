#!/usr/bin/env bash
# Real-KVM acceptance on a disposable kind cluster, for a CI runner with /dev/kvm. Usage:
#   acceptance.sh <plugin OCI archive> <kvm-smoke binary>
# It changes the runner's /dev/kvm mode and creates and deletes a kind cluster, so run it only on a throwaway machine.
# Each section proves one documented behaviour; any failure dumps the cluster's state and exits non-zero.
set -Eeuo pipefail

plugin_archive=$1
smoke_binary=$2
cluster=kvm-acceptance
node=$cluster-control-plane
resource=devices.analogsea.ca/kvm
namespace=kvm-acceptance
kind_image=docker.io/kindest/node:v1.37.0@sha256:a1ed56cfb0e7b93589bdf97c8cd566405a265939e3620fc4f5de89adff580ae5
pause_image=registry.k8s.io/pause:3.10.1@sha256:278fb9dbcca9518083ad1e11276933a2e96f23de604a3a08cc3c80002767d24c
root=$(cd "$(dirname "$0")/.." && pwd)
work=$(mktemp -d)

note() { echo "$*"; echo "- $*" >> "${GITHUB_STEP_SUMMARY:-/dev/null}"; }
# Matches text already captured: grep -q reading a pipe could stop its writer early, which pipefail counts as failure.
contains() { grep -q -- "$1" <<< "$2"; }

dump() {
  echo "::group::cluster state"
  kubectl get nodes,pods -A -o wide || true
  kubectl describe node "$node" || true
  kubectl -n "$namespace" describe pods || true
  kubectl -n kube-system logs -l app.kubernetes.io/name=kvm-device-plugin --tail=200 --prefix || true
  docker exec "$node" journalctl -u kubelet --since -5m --no-pager | tail -200 || true
  # What the runtime asked for: the last kvm-smoke container's devices and user.
  docker exec "$node" sh -c 'crictl inspect "$(crictl ps -a --name kvm-smoke -q | head -1)"' \
    | jq '.info.runtimeSpec.linux.devices, .info.runtimeSpec.process.user' || true
  sudo dmesg | grep -iE 'apparmor="DENIED"' | tail -20 || true
  echo "::endgroup::"
}

fail() {
  echo "::error::$*"
  dump
  exit 1
}
trap 'fail "command failed at line $LINENO"' ERR

# Waits up to $1 seconds for the command after the description $2 to succeed, and reports how long it took.
wait_for() {
  local timeout=$1 what=$2 start=$SECONDS
  shift 2
  until "$@" > /dev/null 2>&1; do
    if ((SECONDS - start >= timeout)); then
      fail "timed out after ${timeout}s waiting for $what"
    fi
    sleep 1
  done
  note "$what after $((SECONDS - start))s"
}

quantity() { kubectl get node "$node" -o "jsonpath={.status.$1['${2//./\\.}']}"; }
allocatable_is() { [ "$(quantity allocatable "${2:-$resource}")" = "$1" ]; }
capacity_is() { [ "$(quantity capacity "${2:-$resource}")" = "$1" ]; }
both_running() {
  [ "$(kubectl -n "$namespace" get pods "$1" "$2" -o 'jsonpath={.items[*].status.phase}')" = 'Running Running' ]
}
unschedulable() {
  kubectl -n "$namespace" get events --field-selector "involvedObject.name=$1" | grep -q "Insufficient $resource"
}
plugin_pod() { kubectl -n kube-system get pods -l app.kubernetes.io/name=kvm-device-plugin -o "jsonpath={.items[0].$1}"; }
node_ready() { [ "$(kubectl get node "$node" -o 'jsonpath={.status.conditions[?(@.type=="Ready")].status}')" = True ]; }
node_kvm() { docker exec "$node" "$@"; }
# The plugin pod's whole log; `kubectl logs -l` would show only its last lines.
plugin_log() { kubectl -n kube-system logs "$(plugin_pod metadata.name)"; }
registrations() { plugin_log | grep -c 'registered .* as' || true; }
registered_more_than() { [ "$(registrations)" -gt "$1" ]; }

# Starts a Pod in $namespace that runs kvm-smoke with the resource. Extra lines are added to the pod's spec, and
# the third argument is kvm-smoke's arguments, as a YAML list.
consumer() {
  local name=$1 extra_spec=${2:-} args=${3:-[]}
  kubectl -n "$namespace" apply -f - > /dev/null <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
spec:
  restartPolicy: Never
  securityContext:
    runAsNonRoot: true
    runAsUser: 65532
    runAsGroup: 65532
    seccompProfile: {type: RuntimeDefault}
$extra_spec
  containers:
    - name: kvm-smoke
      image: kvm-smoke:acceptance
      imagePullPolicy: Never
      args: $args
      securityContext:
        allowPrivilegeEscalation: false
        capabilities: {drop: [ALL]}
      resources:
        limits: {$resource: 1}
EOF
}

# A running container's credentials, read on the node, where crictl's PID is meaningful. $1 is the container's name.
credentials() {
  local pid
  pid=$(node_kvm crictl inspect "$(node_kvm crictl ps --name "^$1\$" -q)" | jq -r .info.pid)
  node_kvm grep -E '^(Uid|CapInh|CapPrm|CapEff|CapBnd|CapAmb|NoNewPrivs|Seccomp):' "/proc/$pid/status"
}
# One field of credentials' output, given as $2.
field() { awk -v name="$1:" '$1 == name { print $2 }' <<< "$2"; }

# The guest loop that runs through sections 7 and 8: how many guests it has run, and a check that the same pod is
# still running and runs another guest after the disruption $1, counted from when this check starts.
guest_runs() { grep -c 'the guest ran' <<< "$(kubectl -n "$namespace" logs runner)" || true; }
runs_more_than() { [ "$(guest_runs)" -gt "$1" ]; }
expect_runner_undisturbed() {
  local runs
  [ "$(kubectl -n "$namespace" get pod runner -o 'jsonpath={.metadata.uid} {.status.phase}')" = "$runner_uid Running" ] \
    || { kubectl -n "$namespace" logs runner | tail -5 || true; fail "the running guest loop stopped $1"; }
  runs=$(guest_runs)
  wait_for 10 "the running guest loop runs a new guest $1, guest number $((runs + 1))," runs_more_than "$runs"
}

# Starts a Pod that holds one unit of the resource until deleted.
holder() {
  kubectl -n "$namespace" apply -f - > /dev/null <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $1
spec:
  securityContext:
    runAsNonRoot: true
    runAsUser: 65535
    seccompProfile: {type: RuntimeDefault}
  containers:
    - name: pause
      image: $pause_image
      securityContext:
        allowPrivilegeEscalation: false
        capabilities: {drop: [ALL]}
      resources:
        limits: {$resource: 1}
EOF
}

finished() {
  local phase
  phase=$(kubectl -n "${2:-$namespace}" get pod "$1" -o 'jsonpath={.status.phase}')
  [ "$phase" = Succeeded ] || [ "$phase" = Failed ]
}

# Runs a kvm-smoke Pod to completion and checks its exit status.
expect_smoke() {
  local name=$1 status=$2 extra_spec=${3:-} actual
  consumer "$name" "$extra_spec"
  wait_for 120 "$name finished" finished "$name"
  actual=$(kubectl -n "$namespace" get pod "$name" -o 'jsonpath={.status.containerStatuses[0].state.terminated.exitCode}')
  if [ "$actual" != "$status" ]; then
    kubectl -n "$namespace" logs "$name" || true
    fail "$name exited $actual, expected $status"
  fi
  if [ "$status" = 0 ]; then
    contains 'the guest ran and wrote 0x2a' "$(kubectl -n "$namespace" logs "$name")"
  fi
  kubectl -n "$namespace" delete pod "$name" --wait=false > /dev/null
  note "$name exited $status as expected"
}

# A plugin that recovers in place keeps its pod: a crash and restart would also restore the resource.
expect_same_plugin() {
  [ "$(plugin_pod metadata.uid)" = "$1" ] || fail "the plugin pod was replaced"
  [ "$(plugin_pod 'status.containerStatuses[0].restartCount')" = 0 ] || fail "the plugin restarted"
}

echo "::group::host"
# Lets the runner's own user run kvm-smoke. The kind node's /dev is a separate copy that keeps the device's original
# mode, which the permission tests below rely on.
echo 'KERNEL=="kvm", GROUP="kvm", MODE="0666", OPTIONS+="static_node=kvm"' \
  | sudo tee /etc/udev/rules.d/99-kvm-acceptance.rules > /dev/null
sudo udevadm control --reload-rules
sudo udevadm trigger --name-match=kvm
wait_for 10 "the runner's /dev/kvm is mode 0666" test "$(stat -c %a /dev/kvm)" = 666
chmod +x "$smoke_binary"
contains 'the guest ran and wrote 0x2a' "$("$smoke_binary")"
note "kvm-smoke ran a guest on the runner"
mkdir "$work/smoke"
cp "$smoke_binary" "$work/smoke/kvm-smoke"
printf 'FROM scratch\nCOPY kvm-smoke /kvm-smoke\nUSER 65532:65532\nENTRYPOINT ["/kvm-smoke"]\n' > "$work/smoke/Containerfile"
docker build --quiet --tag kvm-smoke:acceptance --file "$work/smoke/Containerfile" "$work/smoke" > /dev/null
status=0
docker run --rm kvm-smoke:acceptance || status=$?
[ "$status" = 3 ] || fail "kvm-smoke without a device exited $status, expected 3"
note "kvm-smoke without a device exits 3"
echo "::endgroup::"

echo "::group::cluster"
kind create cluster --name "$cluster" --image "$kind_image" --wait 180s
host_inode=$(stat -c %i /dev/kvm)
[ "$(node_kvm stat -c %i /dev/kvm)" != "$host_inode" ] || fail "the kind node shares the runner's /dev/kvm"
note "cgroup filesystem on the node: $(node_kvm stat -fc %T /sys/fs/cgroup)"
note "the node's /dev/kvm: $(node_kvm stat -c 'mode %a, owner %u:%g' /dev/kvm)"
note "the kvm group: on the runner $(getent group kvm || echo none), on the node $(node_kvm getent group kvm || echo none)"
skopeo copy "oci-archive:$plugin_archive" "docker-archive:$work/plugin.tar:kvm-device-plugin:acceptance"
kind load image-archive "$work/plugin.tar" --name "$cluster"
kind load docker-image kvm-smoke:acceptance --name "$cluster"
kubectl label node "$node" kvm-device-plugin.analogsea.ca/enabled=true
kubectl create namespace "$namespace"
kubectl label namespace "$namespace" pod-security.kubernetes.io/enforce=restricted
overlay="$work/overlay"
mkdir "$overlay"
cat > "$overlay/kustomization.yaml" <<EOF
apiVersion: kustomize.config.k8s.io/v1beta1
kind: Kustomization
resources:
  - $(realpath --relative-to="$overlay" "$root/deploy")
images:
  - name: ghcr.io/analogsea/kvm-device-plugin
    newName: kvm-device-plugin
    newTag: acceptance
patches:
  - target: {kind: DaemonSet, name: kvm-device-plugin}
    patch: |
      - op: add
        path: /spec/template/spec/containers/0/args/-
        value: --slots=2
      - op: add
        path: /spec/template/spec/containers/0/imagePullPolicy
        value: Never
EOF
kubectl apply -k "$overlay"
echo "::endgroup::"

echo "::group::1. the plugin registers and finds KVM through the node's /dev"
wait_for 120 "2 slots allocatable" allocatable_is 2
contains 'KVM is healthy' "$(plugin_log)"
plugin_uid=$(plugin_pod metadata.uid)
# What the privileged plugin really holds, which the README states for containerd.
plugin_credentials=$(credentials kvm-device-plugin)
note "$(node_kvm containerd --version | cut -d' ' -f1-3); the plugin's credentials: $(tr '\t\n' ' ;' <<< "$plugin_credentials")"
[ "$(field Uid "$plugin_credentials")" = 0 ] || fail "the plugin does not run as root"
# The kind node is itself a privileged container, so its init holds every capability the kernel has.
all_capabilities=$(field CapBnd "$(node_kvm grep '^CapBnd:' /proc/1/status)")
[ "$(field CapEff "$plugin_credentials")" = "$all_capabilities" ] \
  && [ "$(field CapBnd "$plugin_credentials")" = "$all_capabilities" ] \
  || fail "the plugin does not hold every capability ($all_capabilities), so the README is out of date"
echo "::endgroup::"

echo "::group::2. the device's permissions apply in an ordinary container, which then runs a guest"
group=$(node_kvm stat -c %g /dev/kvm)
[ "$group" != 0 ] || fail "the node's /dev/kvm belongs to group 0, so the group test would prove nothing"
node_kvm chmod 0660 /dev/kvm
expect_smoke without-group 5
expect_smoke with-group 0 "    supplementalGroups: [$group]"
note "with mode 0660, a non-root container needs the device's group ($group)"
node_kvm chmod 0666 /dev/kvm
expect_smoke consumer 0
echo "::endgroup::"

echo "::group::3. without the resource the device cgroup denies the device"
kubectl apply -f - > /dev/null <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: control
  namespace: default
spec:
  restartPolicy: Never
  securityContext: {runAsUser: 65532, runAsGroup: 65532}
  containers:
    - name: kvm-smoke
      image: kvm-smoke:acceptance
      imagePullPolicy: Never
      volumeMounts: [{name: kvm, mountPath: /dev/kvm}]
  volumes: [{name: kvm, hostPath: {path: /dev/kvm, type: CharDevice}}]
EOF
wait_for 120 "control finished" finished control default
status=$(kubectl get pod control -o 'jsonpath={.status.containerStatuses[0].state.terminated.exitCode}')
[ "$status" = 6 ] || fail "a container given /dev/kvm without the resource exited $status, expected 6 (EPERM)"
note "a container given /dev/kvm without the resource is denied by the device cgroup (exit 6)"
echo "::endgroup::"

echo "::group::4. slots cap concurrent holders"
holder holder-1
holder holder-2
wait_for 120 "two holders running" both_running holder-1 holder-2
consumer over-cap
wait_for 60 "a third consumer to be unschedulable" unschedulable over-cap
kubectl -n "$namespace" delete pod holder-2 over-cap > /dev/null
echo "::endgroup::"

echo "::group::5. losing KVM makes the slots unhealthy without disturbing running containers"
kvm_group=$(node_kvm stat -c %g /dev/kvm)
node_kvm rm /dev/kvm
wait_for 40 "no slots allocatable after KVM disappears" allocatable_is 0
[ -c /dev/kvm ] || fail "removing the node's /dev/kvm removed the runner's"
[ "$(kubectl -n "$namespace" get pod holder-1 -o 'jsonpath={.status.phase}')" = Running ] || fail "holder-1 stopped"
node_kvm mknod -m 0666 /dev/kvm c 10 232
node_kvm chgrp "$kvm_group" /dev/kvm
wait_for 40 "2 slots allocatable after KVM returns" allocatable_is 2
expect_same_plugin "$plugin_uid"
kubectl -n "$namespace" delete pod holder-1 > /dev/null
expect_smoke after-kvm-returns 0
echo "::endgroup::"

echo "::group::6. without privileged mode the plugin cannot open KVM"
mkdir "$work/unprivileged"
cat > "$work/unprivileged/kustomization.yaml" <<'EOF'
apiVersion: kustomize.config.k8s.io/v1beta1
kind: Kustomization
resources:
  - ../overlay
nameSuffix: -unprivileged
labels:
  - pairs: {app.kubernetes.io/name: kvm-device-plugin-unprivileged}
    includeSelectors: true
    includeTemplates: true
patches:
  - target: {kind: DaemonSet}
    patch: |
      - op: replace
        path: /spec/template/spec/containers/0/securityContext/privileged
        value: false
      - op: add
        path: /spec/template/spec/containers/0/args/-
        value: --resource-name=example.com/kvm-unprivileged
EOF
kubectl apply -k "$work/unprivileged"
wait_for 120 "the unprivileged plugin registers 2 slots" capacity_is 2 example.com/kvm-unprivileged
allocatable_is 0 example.com/kvm-unprivileged || fail "the unprivileged plugin's slots are allocatable"
contains 'Operation not permitted' "$(kubectl -n kube-system logs daemonset/kvm-device-plugin-unprivileged)"
note "without privileged mode, the plugin registers but KVM is unhealthy: Operation not permitted"
kubectl -n kube-system delete daemonset kvm-device-plugin-unprivileged --wait=true > /dev/null
echo "::endgroup::"

echo "::group::7. the plugin re-registers when kubelet restarts, and a running guest loop is undisturbed"
# Runs a guest every second, opening /dev/kvm each time, through this section and the next; any failure ends it.
consumer runner "" '["--every", "1"]'
wait_for 60 "the running guest loop's first guest" runs_more_than 0
runner_uid=$(kubectl -n "$namespace" get pod runner -o 'jsonpath={.metadata.uid}')
runner_credentials=$(credentials kvm-smoke)
note "a consumer's credentials: $(tr '\t\n' ' ;' <<< "$runner_credentials")"
[ "$(field CapEff "$runner_credentials")" = 0000000000000000 ] || fail "a consumer holds capabilities"
before=$(registrations)
node_kvm systemctl restart kubelet
# The node's status is stale until kubelet is back, so wait for the plugin's fresh registration first.
wait_for 60 "the plugin registers again after kubelet restarts" registered_more_than "$before"
contains 'registering again' "$(plugin_log)"
wait_for 120 "the node is ready after kubelet restarts" node_ready
wait_for 60 "2 slots allocatable after kubelet restarts" allocatable_is 2
expect_same_plugin "$plugin_uid"
expect_runner_undisturbed "after kubelet restarts"
expect_smoke after-kubelet-restart 0
echo "::endgroup::"

echo "::group::8. a new plugin pod can start before the old one stops, and a running guest loop is undisturbed"
old_pod=$(plugin_pod metadata.name)
(
  trap - ERR
  kubectl -n kube-system logs -f --timestamps "$old_pod" > "$work/old-plugin.log" 2>&1 || true
) &
# The new pod counts as available only after minReadySeconds, so the old one outlives its registration.
kubectl -n kube-system patch daemonset kvm-device-plugin --type merge -p \
  '{"spec":{"minReadySeconds":10,"updateStrategy":{"rollingUpdate":{"maxSurge":1,"maxUnavailable":0}}}}'
kubectl -n kube-system rollout restart daemonset kvm-device-plugin
kubectl -n kube-system rollout status daemonset kvm-device-plugin --timeout 180s
wait_for 60 "2 slots allocatable after a surge rollout" allocatable_is 2
wait_for 30 "the old plugin stopped" grep -q stopping "$work/old-plugin.log"
# The old pod can still be terminating, so find the other one.
new_pod=""
for pod in $(kubectl -n kube-system get pods -l app.kubernetes.io/name=kvm-device-plugin -o 'jsonpath={.items[*].metadata.name}'); do
  [ "$pod" = "$old_pod" ] || new_pod=$pod
done
[ -n "$new_pod" ] || fail "the rollout kept the old pod"
kubectl -n kube-system logs --timestamps "$new_pod" > "$work/new-plugin.log"
registered=$(grep -m1 'registered .* as' "$work/new-plugin.log" | cut -d' ' -f1)
stopped=$(grep -m1 stopping "$work/old-plugin.log" | cut -d' ' -f1)
[ -n "$registered" ] && [ -n "$stopped" ] || fail "missing registration or stop log lines"
(($(date -d "$registered" +%s%N) < $(date -d "$stopped" +%s%N))) \
  || fail "the new plugin registered at $registered, after the old one stopped at $stopped"
note "the new plugin registered at $registered, before the old one stopped at $stopped"
expect_runner_undisturbed "after a rollout"
note "the running guest loop ran $(guest_runs) guests, each opening /dev/kvm, through a kubelet restart and a rollout"
kubectl -n "$namespace" delete pod runner --wait=false > /dev/null
expect_smoke after-rollout 0
echo "::endgroup::"

kind delete cluster --name "$cluster"
rm -rf "$work"
note "acceptance passed"
