#!/bin/bash
# Run mini-container inside a Lima VM, for hosts without the Linux kernel features it needs.
set -euo pipefail

VM_NAME="${MINI_CONTAINER_VM:-mini-container}"
REPO_DIR="$(cd "$(dirname "$0")/.." && pwd)"
TEMPLATE="$REPO_DIR/lima/mini-container.yaml"
# Kept on the VM disk: APFS is case-insensitive by default, so a Linux rootfs
# extracted on the macOS side can lose files.
GUEST_ROOTFS="/opt/mini-container/rootfs"

usage() {
  cat <<EOF
Usage:
  scripts/lima.sh rootfs            Download an Alpine rootfs to $GUEST_ROOTFS in the VM
  scripts/lima.sh run RUN_ARGS...   Build in the VM, then run 'mini-container run RUN_ARGS...'

Example:
  scripts/lima.sh rootfs
  scripts/lima.sh run $GUEST_ROOTFS /bin/sh

Environment:
  MINI_CONTAINER_VM   Lima instance name (default: mini-container)
EOF
}

die() {
  echo "lima.sh: $*" >&2
  exit 1
}

ensure_vm() {
  command -v limactl >/dev/null || die "limactl not found; install Lima 2.0 or later"
  case "$REPO_DIR/" in
    "$HOME"/*) ;;
    *) die "the repository must be under $HOME, because the VM mounts only the home directory" ;;
  esac

  local status
  status="$(limactl list --format '{{.Status}}' "$VM_NAME" 2>/dev/null || true)"
  case "$status" in
    Running) ;;
    "") limactl start --tty=false --name="$VM_NAME" "$TEMPLATE" ;;
    *) limactl start --tty=false "$VM_NAME" ;;
  esac
}

in_vm() {
  limactl shell --workdir "$REPO_DIR" "$VM_NAME" "$@"
}

fetch_rootfs() {
  in_vm bash -c '
    set -euo pipefail
    sudo install -d -o "$(id -u)" -g "$(id -g)" "$(dirname "$1")"
    bash ./scripts/fetch-rootfs.sh "$1"
  ' bash "$GUEST_ROOTFS"
}

run_container() {
  # The home directory is mounted read-only, so the build output goes to the VM disk.
  in_vm bash -c '
    set -euo pipefail
    cargo="$HOME/.cargo/bin/cargo"
    [ -x "$cargo" ] || { echo "lima.sh: cargo is not installed in the VM" >&2; exit 1; }
    export CARGO_TARGET_DIR="$HOME/.cache/mini-container/target"
    "$cargo" build --release --locked --quiet
    exec sudo "$CARGO_TARGET_DIR/release/mini-container" run "$@"
  ' bash "$@"
}

case "${1:-}" in
  rootfs)
    [ $# -eq 1 ] || die "rootfs takes no arguments"
    ensure_vm
    fetch_rootfs
    ;;
  run)
    shift
    [ $# -gt 0 ] || { usage >&2; exit 2; }
    ensure_vm
    run_container "$@"
    ;;
  -h | --help)
    usage
    ;;
  *)
    usage >&2
    exit 2
    ;;
esac
