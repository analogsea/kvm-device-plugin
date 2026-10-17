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

# Starts a Pod in $namespace that runs kvm-smoke with the resource. Extra lines are added to the pod's spec.
consumer() {
  local name=$1 extra_spec=${2:-}
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
      securityContext:
        allowPrivilegeEscalation: false
        capabilities: {drop: [ALL]}
      resources:
        limits: {$resource: 1}
EOF
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
    kubectl -n "$namespace" logs "$name" | grep -q 'the guest ran and wrote 0x2a'
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
"$smoke_binary" | grep -q 'the guest ran and wrote 0x2a'
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
kubectl -n kube-system logs -l app.kubernetes.io/name=kvm-device-plugin | grep -q 'KVM is healthy'
plugin_uid=$(plugin_pod metadata.uid)
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
kubectl -n kube-system logs daemonset/kvm-device-plugin-unprivileged | grep -q 'Operation not permitted'
note "without privileged mode, the plugin registers but KVM is unhealthy: Operation not permitted"
kubectl -n kube-system delete daemonset kvm-device-plugin-unprivileged --wait=true > /dev/null
echo "::endgroup::"

echo "::group::7. the plugin re-registers when kubelet restarts"
node_kvm systemctl restart kubelet
wait_for 120 "the node is ready after kubelet restarts" node_ready
wait_for 60 "2 slots allocatable after kubelet restarts" allocatable_is 2
expect_same_plugin "$plugin_uid"
kubectl -n kube-system logs -l app.kubernetes.io/name=kvm-device-plugin | grep -q 'registering again'
expect_smoke after-kubelet-restart 0
echo "::endgroup::"

echo "::group::8. a new plugin pod can start before the old one stops"
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
new_pod=$(plugin_pod metadata.name)
[ "$new_pod" != "$old_pod" ] || fail "the rollout kept the old pod"
registered=$(kubectl -n kube-system logs --timestamps "$new_pod" | grep -m1 'registered .* as' | cut -d' ' -f1)
stopped=$(grep -m1 stopping "$work/old-plugin.log" | cut -d' ' -f1)
[ -n "$registered" ] && [ -n "$stopped" ] || fail "missing registration or stop log lines"
(($(date -d "$registered" +%s%N) < $(date -d "$stopped" +%s%N))) \
  || fail "the new plugin registered at $registered, after the old one stopped at $stopped"
note "the new plugin registered at $registered, before the old one stopped at $stopped"
expect_smoke after-rollout 0
echo "::endgroup::"

kind delete cluster --name "$cluster"
rm -rf "$work"
note "acceptance passed"
