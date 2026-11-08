#!/bin/bash
set -euo pipefail

# Use the first CLI argument if provided, else the default rootfs that `aj run` uses.
# The XDG Base Directory specification says to ignore a relative XDG_DATA_HOME.
DATA_HOME="${XDG_DATA_HOME:-}"
[[ "$DATA_HOME" == /* ]] || DATA_HOME="$HOME/.local/share"
ROOTFS_DIR="${1:-$DATA_HOME/agent-jail/images/default}"
VERSION="3.24.2"
ARCH="$(uname -m)"
URL="https://dl-cdn.alpinelinux.org/alpine/v${VERSION%.*}/releases/${ARCH}/alpine-minirootfs-${VERSION}-${ARCH}.tar.gz"

# If the rootfs already exists and has contents, skip the following instructions
if [ -d "$ROOTFS_DIR" ] && [ -n "$(ls -A "$ROOTFS_DIR" 2>/dev/null)" ]; then
  echo "rootfs exists at $ROOTFS_DIR -> skipping"
  exit 0
fi

mkdir -p "$ROOTFS_DIR"
echo "fetching $URL"

# --no-same-owner: Forces extracted files to be owned by the user running the command (host: me, container: root)
# --same-permissions: Keep the permissions in the archive instead of applying the user's umask (default value: 022),
#   so that /tmp keeps the sticky bit and stays writable for every user (1777)
curl -fsSL "$URL" | tar -xz --no-same-owner --same-permissions -C "$ROOTFS_DIR"
echo "extracted alpine-minirootfs to $ROOTFS_DIR"
