# Develop on macOS

`aj` does not build or run on macOS, but you can work on it in a Linux virtual machine (VM).
[Lima](https://lima-vm.io) runs the VM and shares your macOS home directory with it.
The VM can read these files, but it cannot change them.
These steps were tested with Lima 2.3.0 and Debian 13 on an Apple silicon Mac.
They do not use Ubuntu 24.04, because its default AppArmor settings restrict user namespaces that work without root access.

1. Install Lima.

   ```bash
   brew install lima
   ```

2. Create and start a Debian 13 VM named `aj`.

   ```bash
   limactl start --name=aj template:debian-13
   ```

3. Install the build tools and the system packages in the VM.

   ```bash
   limactl shell aj sh -c 'sudo apt-get update && sudo apt-get install -y build-essential uidmap passt'
   ```

4. Install Rust in the VM.

   ```bash
   limactl shell aj sh -c 'curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y'
   ```

5. Keep the build files inside the VM, because the VM cannot write to the project directory.

   ```bash
   limactl shell aj sh -c 'echo "export CARGO_TARGET_DIR=\$HOME/target" >> ~/.profile'
   ```

6. In the project directory on macOS, build and install `aj` in the VM.

   ```bash
   limactl shell aj cargo install --path . --locked
   ```

7. Download the rootfs in the VM.

   ```bash
   limactl shell aj ./scripts/fetch-rootfs.sh
   ```

8. Start a shell in the container.

   ```bash
   limactl shell aj aj run -- /bin/sh
   ```

`limactl shell aj COMMAND` runs a command in the VM, in the same directory as your macOS terminal.
Add `limactl shell aj` before the other commands in this README, for example `limactl shell aj make lint`.
Do not use `~` in these commands, because the macOS shell changes it to your macOS home directory first.
To use a path in the VM, put the command in `sh -c '...'`.
Run `limactl shell aj` to open a shell in the VM, and `limactl stop aj` to stop the VM.

- Run `make fmt` on macOS, because it changes the source files.
- Files shared from your macOS home directory are read-only in the container, even without `:ro`. To test writable shared files, use a directory inside the VM.
- The rootfs and the files for each run are stored in the VM.

To check the code on macOS without the VM, add a Linux target.
On an Intel Mac, use `x86_64-unknown-linux-gnu` instead.

```bash
rustup target add aarch64-unknown-linux-gnu
cargo check --all-targets --target aarch64-unknown-linux-gnu
```

`cargo check` and `cargo clippy` work with this target. `cargo build` and `cargo test` do not.
For rust-analyzer, set `rust-analyzer.cargo.target` to the same target.
