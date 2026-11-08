# agent-jail

This project is for developers who do not want AI agents to fill their system with stray files and extra dependencies.
It gives AI agents an isolated environment for their work, and it runs without root privileges.

## Requirements

This project runs only on Linux. macOS support is planned for a later version.

- cgroup v2. Debian 11 and later, and Ubuntu 22.04 and later, use cgroup v2 by default.

- Unprivileged user namespaces. Run `unshare -Ur id`, and the output must start with `uid=0(root)`.

- `newuidmap` and `newgidmap` from the `uidmap` package. Run `command -v newuidmap newgidmap`, and the output must show both paths. On Debian or Ubuntu, install them with `sudo apt install uidmap`.

- A range of at least 65536 IDs for your user in `/etc/subuid` and `/etc/subgid`. Run `grep "^$(id -un):" /etc/subuid /etc/subgid`, and each file must show a line such as `jace:100000:65536`. `useradd` usually adds these lines. If they are missing, add them with `sudo usermod --add-subuids 100000-165535 --add-subgids 100000-165535 "$(id -un)"`. Choose a range that no other user has.

- systemd as PID 1 with a user instance for your user. Run `systemctl --user is-system-running`, and the output must be `running` or `degraded`. The runtime asks this instance for a cgroup that you can manage.

- Rust 1.85 or newer, because the project uses the 2024 edition. The project was last checked with Rust 1.98.1.

- `curl` and `tar`, because `scripts/fetch-rootfs.sh` downloads and extracts an Alpine minirootfs.

## How to run

### Run directly on the host

1. Fetch the default rootfs into `~/.local/share/agent-jail/images/default`. If `XDG_DATA_HOME` is set, the script uses `$XDG_DATA_HOME/agent-jail/images/default` instead. The script skips the download if the directory already has contents.

```bash
./scripts/fetch-rootfs.sh
```

2. Build the binary.

```bash
cargo build --release
```

3. Run the binary.

```bash
./target/release/aj run --cpus 1.0 --mem 536870912 --pids 1024 -- /bin/sh
```

To use another rootfs, fetch it into a directory of your choice and pass that directory with `--rootfs`.

```bash
./scripts/fetch-rootfs.sh ./rootfs
./target/release/aj run --rootfs ./rootfs -- /bin/sh
```

### Work on a host project

Each run starts from the rootfs and writes its changes to a separate directory under `~/.local/share/agent-jail/runs`. Packages that a run installs therefore never change the rootfs. The runtime removes the directory when the run ends.

Use `-v HOST[:CONTAINER][:ro]` to make a host file or directory visible in the container. Without `CONTAINER`, the container path is the same as the host path. Add `:ro` to make it read-only. Use `-w` to choose the working directory. Without `-w`, the program starts in your current directory if a volume contains it, and in `/` otherwise.

```bash
cd ~/my-project
~/development/agent-jail/target/release/aj run -v . -- make test
~/development/agent-jail/target/release/aj run -v ./data:/data:ro -w /data -- ls
```

The host shell handles `&&`, `;`, and `|` before `aj` starts. In `aj run -- make && make test`, only `make` runs in the container, and `make test` runs on the host. To run several commands in the container, pass them to a shell in the container:

```bash
~/development/agent-jail/target/release/aj run -v . -- /bin/sh -c 'make && make test'
```

Programs in the container can read their own limits in `/sys/fs/cgroup`, for example `memory.max`, `cpu.max`, and `pids.max`.

### Run with an OCI bundle

The `run-bundle` command reads `config.json` from a bundle directory and runs the program that the file describes.

Like `run`, it waits until the program finishes.

1. Fetch the rootfs into the example bundle.

```bash
./scripts/fetch-rootfs.sh ./examples/oci-bundle/rootfs
```

2. Run the example bundle.

```bash
./target/release/aj run-bundle ./examples/oci-bundle
```

The program prints `OCI_STARTED`, its working directory, its user, and its environment.

The environment contains only the entries in `process.env`.

### Development commands

The Makefile has only formatting and linting targets.

```bash
make fmt    # cargo fmt
make lint   # cargo clippy --all-features
```
