# Container Runtime

This repository contains a study-purpose container runtime written in Rust.
It isolates the filesystem, processes, network, user namespace, and hardware resources.

## Requirements

This project only runs on Linux. It uses namespaces, cgroup v2, `pivot_root`, and `mount` calls. It runs as a normal user and refuses to run as root.

- A Linux host with cgroup v2 mounted in unified mode. Run `stat -fc %T /sys/fs/cgroup`, and the output must be `cgroup2fs`. Debian 11 and later, and Ubuntu 22.04 and later, use cgroup v2 by default.
- Unprivileged user namespaces. Run `unshare -Ur id`, and the output must start with `uid=0(root)`.
- `newuidmap` and `newgidmap` from the `uidmap` package. Run `command -v newuidmap newgidmap`, and the output must show both paths. On Debian or Ubuntu, install them with `sudo apt install uidmap`.
- A range of at least 65536 IDs for your user in `/etc/subuid` and `/etc/subgid`. Run `grep "^$(id -un):" /etc/subuid /etc/subgid`, and each file must show a line such as `jace:100000:65536`. `useradd` usually adds these lines. If they are missing, run `sudo usermod --add-subuids 100000-165535 --add-subgids 100000-165535 "$(id -un)"`, and choose a range that no other user has.
- systemd as PID 1 with a user instance for your user. Run `systemctl --user is-system-running`, and the output must be `running` or `degraded`. The runtime asks this instance for a cgroup that you can manage.
- A Rust toolchain that supports edition 2024, so Rust 1.85 or newer. The project was last checked with Rust 1.98.1.
- `curl` and `tar`, because `scripts/fetch-rootfs.sh` downloads and extracts an Alpine minirootfs.

## How to run

### Run directly on the host

1. Fetch the rootfs. The script skips the download if `./rootfs` already has contents.

```bash
./scripts/fetch-rootfs.sh ./rootfs
```

2. Build the binary.

```bash
cargo build --release
```

3. Run the binary.

```bash
./target/release/container-runtime run ./rootfs \
  --cpus 1.0 --mem 536870912 --pids 1024 /bin/sh
```

Notes about this command:

- The runtime needs a cgroup that systemd delegates to you. If it starts in another cgroup, it runs itself again through `systemd-run --user --scope -p Delegate=yes`. Each run therefore appears as a `container-runtime-*.scope` unit in `systemctl --user list-units`.
- Container root maps to the user who runs the runtime, so files created inside the container stay writable for you.
- Container IDs 1 to 65536 map to your range in `/etc/subuid` and `/etc/subgid`. For example, with `jace:100000:65536`, container UID 42 is host UID 100041. You cannot change files that such IDs own, or remove anything inside directories that they own. Remove them inside the container instead, for example `./target/release/container-runtime run ./rootfs /bin/rm -rf /path/in/rootfs`.

### Run an OCI bundle

The `run-bundle` command reads `config.json` from a bundle directory and runs its program. It waits until the program finishes, like `run`.

1. Fetch the rootfs into the example bundle.

```bash
./scripts/fetch-rootfs.sh ./examples/oci-bundle/rootfs
```

2. Run the example bundle.

```bash
./target/release/container-runtime run-bundle ./examples/oci-bundle
```

The program prints `OCI_STARTED`, its working directory, its user, and its environment. The environment contains only the entries in `process.env`.

Notes about bundles:

- The runtime supports only part of the OCI runtime specification 1.3.0. If `config.json` requests a setting that the runtime cannot apply, the command fails before it creates the container, and the error names the field. Unknown fields are ignored.
- `process.args[0]` must be an absolute path.
- The runtime maps container root to the user who runs it, and container IDs 1 to 65536 to that user's range in `/etc/subuid` and `/etc/subgid`. A bundle may leave out `linux.uidMappings` and `linux.gidMappings`, as the example does. If it includes them, they must describe exactly this mapping.
- `process.user` may use any mapped container ID, and `process.user.additionalGids` sets the supplementary groups. The program never keeps the supplementary groups of the host user.

### Development commands

The Makefile only holds formatting and linting targets.

```bash
make fmt    # cargo fmt
make lint   # cargo clippy --all-features
```
