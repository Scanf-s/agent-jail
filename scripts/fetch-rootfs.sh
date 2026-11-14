#!/bin/bash
set -euo pipefail

# With a directory argument, extract the rootfs into that directory. Without one, add it to the
# image store, where `images/default` is the rootfs that `aj run` uses by default.
# The XDG Base Directory specification says to ignore a relative XDG_DATA_HOME.
DATA_HOME="${XDG_DATA_HOME:-}"
[[ "$DATA_HOME" == /* ]] || DATA_HOME="$HOME/.local/share"
IMAGES_DIR="$DATA_HOME/agent-jail/images"
VERSION="3.24.2"
ARCH="$(uname -m)"
URL="https://dl-cdn.alpinelinux.org/alpine/v${VERSION%.*}/releases/${ARCH}/alpine-minirootfs-${VERSION}-${ARCH}.tar.gz"

# The checksums that Alpine publishes next to each archive. A pinned checksum detects a changed
# or damaged download, and it names the image before the download.
case "$ARCH" in
  x86_64) SHA256="c5ca053cfe1d85c5b96dff8b9bc57045f7f184a30ffb6b65776409ca90388677" ;;
  aarch64) SHA256="9bf70a7f18ea44094cbb5f70c58f9af129c8214745743db0e68e5502cc2ce773" ;;
  *)
    echo "no pinned checksum for $ARCH" >&2
    exit 1
    ;;
esac

# Download the archive to $1 and verify it.
download() {
  echo "fetching $URL"
  curl -fsSL -o "$1" "$URL"
  echo "$SHA256  $1" | sha256sum --check --quiet -
}

# Extract the archive $1 into the directory $2.
# --no-same-owner: Forces extracted files to be owned by the user running the command (host: me, container: root)
# --same-permissions: Keep the permissions in the archive instead of applying the user's umask (default value: 022),
#   so that /tmp keeps the sticky bit and stays writable for every user (1777)
extract() {
  tar -xz --no-same-owner --same-permissions -f "$1" -C "$2"
}

if [ $# -gt 0 ]; then
  ROOTFS_DIR="$1"
  if [ -d "$ROOTFS_DIR" ] && [ -n "$(ls -A "$ROOTFS_DIR" 2>/dev/null)" ]; then
    echo "rootfs exists at $ROOTFS_DIR -> skipping"
    exit 0
  fi
  mkdir -p "$ROOTFS_DIR"
  archive="$(mktemp)"
  trap 'rm -f "$archive"' EXIT
  download "$archive"
  extract "$archive" "$ROOTFS_DIR"
  echo "extracted alpine-minirootfs to $ROOTFS_DIR"
  exit 0
fi

# An image is never changed after it is stored, so sandboxes that record its ID keep their base.
IMAGE="sha256-$SHA256"
mkdir -p "$IMAGES_DIR"
if [ -d "$IMAGES_DIR/$IMAGE" ]; then
  echo "image $IMAGE exists -> skipping the download"
else
  # The image gets its name only when it is complete, so a failed run leaves no partial image.
  tmp="$(mktemp -d "$IMAGES_DIR/.fetch-XXXXXX")"
  trap 'rm -rf "$tmp"' EXIT
  download "$tmp/archive.tar.gz"
  mkdir "$tmp/rootfs"
  extract "$tmp/archive.tar.gz" "$tmp/rootfs"
  mv -T "$tmp/rootfs" "$IMAGES_DIR/$IMAGE"
  echo "extracted alpine-minirootfs to $IMAGES_DIR/$IMAGE"
fi

DEFAULT="$IMAGES_DIR/default"
if [ "$(readlink "$DEFAULT" 2>/dev/null)" = "$IMAGE" ]; then
  echo "default -> $IMAGE"
  exit 0
fi

# A running container may use the current default as its overlay lower directory, and OverlayFS
# does not allow changes to a lower directory while it is mounted.
running="$(systemctl --user list-units --type=scope --no-legend 'agent-jail-*')"
if [ -n "$running" ]; then
  echo "agent-jail containers are running; end them and run this script again to change $DEFAULT" >&2
  exit 1
fi
# A plain directory from an older version of this script holds a rootfs that can be fetched again.
if [ -d "$DEFAULT" ] && [ ! -L "$DEFAULT" ]; then
  rm -rf "$DEFAULT"
fi
# rename replaces the link at once, so `aj run` never finds no default.
ln -sfn "$IMAGE" "$DEFAULT.new"
mv -T "$DEFAULT.new" "$DEFAULT"
echo "default -> $IMAGE"
