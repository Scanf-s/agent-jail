# Container Runtime

This project is for the developers who want AI Agents don't mess up their system with messy files and from the additional dependencies installation.
This application provides an isolated environment to AI Agents to manage their work in separated environment on rootless mode.

## Requirements

This project only runs on Linux. MacOS will be supported after.

- Debian 11 and later, and Ubuntu 22.04 and later, use cgroup v2 by default.

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

### Run with an OCI bundle

The `run-bundle` command reads `config.json` from a bundle directory and runs its program.

It waits until the program finishes, like `run`.

1. Fetch the rootfs into the example bundle.

```bash
./scripts/fetch-rootfs.sh ./examples/oci-bundle/rootfs
```

2. Run the example bundle.

```bash
./target/release/container-runtime run-bundle ./examples/oci-bundle
```

The program prints `OCI_STARTED`, its working directory, its user, and its environment.

The environment contains only the entries in `process.env`.

### Development commands

The Makefile only holds formatting and linting targets.

```bash
make fmt    # cargo fmt
make lint   # cargo clippy --all-features
```
