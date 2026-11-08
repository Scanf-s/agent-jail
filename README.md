# Container Runtime

This repository contains a study-purpose container runtime written in Rust.
It isolates the filesystem, processes, network, user namespace, and hardware resources.

## Requirements

This project only runs on Linux. It uses namespaces, cgroup v2, `pivot_root`, and privileged `mount` calls.

- A Linux host with cgroup v2 mounted in unified mode. Run `stat -fc %T /sys/fs/cgroup`, and the output must be `cgroup2fs`. Debian 11 and later, and Ubuntu 22.04 and later, use cgroup v2 by default.
- Root privileges. The runtime writes to `/sys/fs/cgroup`, and it also calls `mount` and `pivot_root`.
- A Rust toolchain that supports edition 2024, so Rust 1.85 or newer. The project was last checked with Rust 1.98.1.
- `curl` and `tar`, because `scripts/fetch-rootfs.sh` downloads and extracts an Alpine minirootfs.

## How to run

### Run directly on the host

This is the normal path on a native Debian or Ubuntu machine.

1. Fetch the rootfs. The script skips the download if `./rootfs` already has contents.

```bash
./scripts/fetch-rootfs.sh ./rootfs
```

2. Build the binary as your normal user.

```bash
cargo build --release
```

3. Run the binary with `sudo`.

```bash
sudo ./target/release/container-runtime run ./rootfs \
  --cpus 1.0 --mem 536870912 --pids 1024 \
  --uid "$(id -u)" --gid "$(id -g)" /bin/sh
```

Two notes about this command:

- Avoid `sudo cargo run` when you installed Rust with `rustup`. The root PATH usually does not include `~/.cargo/bin`, so the command fails. Build first, then run the produced binary with `sudo`.
- `--uid` and `--gid` set the host user that container root maps to. `$(id -u)` and `$(id -g)` map it to yourself, so files created inside the container stay writable for you.

### Run an OCI bundle

The `run-bundle` command reads `config.json` from a bundle directory and runs its program. It waits until the program finishes, like `run`.

1. Fetch the rootfs into the example bundle.

```bash
./scripts/fetch-rootfs.sh ./examples/oci-bundle/rootfs
```

2. Run the example bundle with `sudo`.

```bash
sudo ./target/release/container-runtime run-bundle ./examples/oci-bundle
```

The program prints `OCI_STARTED`, its working directory, its user, and its environment. The environment contains only the entries in `process.env`.

Notes about bundles:

- The runtime supports only part of the OCI runtime specification 1.3.0. If `config.json` requests a setting that the runtime cannot apply, the command fails before it creates the container, and the error names the field. Unknown fields are ignored.
- `process.args[0]` must be an absolute path.
- The example maps container root to host UID and GID 1000, like `--uid` and `--gid` in the `run` example. If `id -u` or `id -g` prints another value, change `hostID` in `linux.uidMappings` and `linux.gidMappings`.
- Container init reaches the rootfs from inside the new user namespace. If container root maps to a host user that cannot search every directory above the rootfs, the command fails. For example, this happens when container root maps to host root and the rootfs is under a home directory with mode `750`.

### Development commands

The Makefile only holds formatting and linting targets.

```bash
make fmt    # cargo fmt
make lint   # cargo clippy --all-features
```
