# agent-jail

This project is for developers who do not want AI agents to fill their system with stray files and extra dependencies.
It gives AI agents an isolated environment for their work, and it runs without root privileges.

## Requirements

This project runs only on Linux. macOS support is planned for a later version.
To develop on macOS, see [Develop on macOS](#develop-on-macos).

- cgroup v2. Debian 11 and later, and Ubuntu 22.04 and later, use cgroup v2 by default.

- Unprivileged user namespaces. Run `unshare -Ur id`, and the output must start with `uid=0(root)`.

- `newuidmap` and `newgidmap` from the `uidmap` package. Run `command -v newuidmap newgidmap`, and the output must show both paths. On Debian or Ubuntu, install them with `sudo apt install uidmap`.

- A range of at least 65536 IDs for your user in `/etc/subuid` and `/etc/subgid`. Run `grep "^$(id -un):" /etc/subuid /etc/subgid`, and each file must show a line such as `jace:100000:65536`. `useradd` usually adds these lines. If they are missing, add them with `sudo usermod --add-subuids 100000-165535 --add-subgids 100000-165535 "$(id -un)"`. Choose a range that no other user has.

- `pasta` from the `passt` package, for network access in the container. Run `command -v pasta`, and the output must show its path. On Debian or Ubuntu, install it with `sudo apt install passt`. Without it, only `--net none` works.

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

Use `-v HOST[:CONTAINER][:ro]` to make a host file or directory visible in the container.

Without `CONTAINER`, the container path is the same as the host path.

Add `:ro` to make it read-only. 

Use `-w` to choose the working directory. Without `-w`, the program starts in your current directory if a volume contains it, and in `/` otherwise.

```bash
cd ~/my-project
aj run -v . -- make test
aj run -v ./data:/data:ro -w /data -- ls
aj run -v . -- /bin/sh -c 'make && make test'
```

Programs in the container can read their own limits in `/sys/fs/cgroup`, for example `memory.max`, `cpu.max`, and `pids.max`.

### Environment variables

The container does not receive the host's environment, because it often holds secrets such as API keys.
By default, the program receives only these variables:

| Variable | Value |
| --- | --- |
| `PATH` | `/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin` |
| `HOME` | `/root` |
| `TERM` | The host's value, if it is set |
| `http_proxy`, `https_proxy`, `no_proxy`, and their uppercase forms | The host's values, if they are set |

Use `--env KEY=VALUE` to set a variable, and `--env KEY` to copy the host's value.
If the host does not set `KEY`, the container does not get it.
`--env` can be repeated, and it overrides the defaults.

`LANG` is not copied, because the rootfs may not have the host's locale.
If a program needs a locale, set one that every rootfs has.

```bash
aj run --env LANG=C.UTF-8 -- python3 app.py
aj run --env GITHUB_TOKEN -v . -- gh pr list
```

### Network

Use `--net` to choose the network mode.

| Mode | What the container can reach |
| --- | --- |
| `pasta` (default) | The internet, and its own servers through `localhost`. The runtime writes `/etc/resolv.conf` unless a volume covers it. |
| `none` | Only its own servers through `localhost`. Use it for work that must stay offline. |

```bash
aj run -- apk add python3
aj run --net none -v . -- make test
```

In `pasta` mode, the container cannot reach host services that listen only on `127.0.0.1`.
It can reach host services that listen on other host addresses, including `0.0.0.0`, for example through a Docker bridge address.
Run development services on the host with `127.0.0.1` if the container must not reach them.

The host cannot reach servers in the container yet.

`run-bundle` always uses `none`.

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

## Develop on macOS

The runtime uses Linux kernel features, so it does not build or run on macOS.
On macOS, edit the code as usual, and build and run it in a Linux VM.

These steps use [Lima](https://lima-vm.io), which runs a Linux VM and shares your macOS home directory with it.
The steps were last checked with Lima 2.3.0 and a Debian 13 VM on an Apple silicon Mac.
They use Debian 13 because Ubuntu 24.04 restricts unprivileged user namespaces with AppArmor by default.

### Set up the VM

1. Install Lima.

```bash
brew install lima
```

2. Create and start a Debian 13 VM named `aj`.

```bash
limactl start --name=aj template:debian-13
```

3. Update the package lists in the VM.

```bash
limactl shell aj sudo apt-get update
```

4. Install a C toolchain and `pasta` in the VM.

```bash
limactl shell aj sudo apt-get install -y build-essential passt
```

5. Install Rust in the VM.

```bash
limactl shell aj sh -c 'curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y'
```

6. Make Cargo write its build output in the VM. This keeps Linux build files out of the `target` directory on macOS.

```bash
limactl shell aj sh -c 'echo "export CARGO_TARGET_DIR=\$HOME/target" >> ~/.profile'
```

7. Go to the project directory on macOS.

```bash
cd ~/path/to/agent-jail
```

8. Fetch the default rootfs into the VM.

```bash
limactl shell aj ./scripts/fetch-rootfs.sh
```

To check the other requirements, open a shell in the VM with `limactl shell aj`, and run the commands in [Requirements](#requirements).

### Build and run in the VM

Edit the code on macOS.
Run commands in the VM from the project directory on macOS, with `limactl shell aj` in front of each command.
`limactl shell` runs the command in the same directory in the VM.

```bash
limactl shell aj cargo test
limactl shell aj cargo clippy --all-features
limactl shell aj cargo run --release -- run -- /bin/sh
```

Do not use `~` in these commands, because the macOS shell replaces it with your macOS home directory before the VM gets the command.
To use a path in the VM's home directory, put the command in `sh -c '...'`.

Run `limactl shell aj` to open a shell in the VM.
Run `limactl stop aj` to stop the VM, and `limactl start aj` to start it again.

### Limits of the shared home directory

- The VM can read your macOS home directory, but it cannot write to it. So containers that you test cannot change your macOS files.
- Run `make fmt` on macOS, because it writes to the source files.
- A volume from the shared home directory is read-only in the container, even without `:ro`. To test a writable volume, use a directory in the VM's home directory.
- The VM has its own home directory, so the rootfs and the run directories are stored in the VM.

### Check the code on macOS

Without a Linux target, editors and `cargo check` on macOS report errors for Linux-only code.
To check the code on macOS without the VM, add a Linux target.
On an Intel Mac, use `x86_64-unknown-linux-gnu` instead of `aarch64-unknown-linux-gnu`.

```bash
rustup target add aarch64-unknown-linux-gnu
cargo check --all-targets --target aarch64-unknown-linux-gnu
```

`cargo check` and `cargo clippy` work with this target.
`cargo build` and `cargo test` do not work, because macOS cannot link Linux binaries.

For rust-analyzer, set `rust-analyzer.cargo.target` to the same target.
