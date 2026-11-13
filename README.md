# agent-jail

Run AI agents in Linux containers without root access.
Files and packages added inside a container are deleted after each run.
Changes to shared host files are kept.

## Requirements

Linux is required. On macOS, follow the [Linux virtual machine setup](MACOS.md).

- cgroup v2 and user namespaces enabled for users without root access.
- systemd as PID 1, with a working user service manager.
- `newuidmap` and `newgidmap` from the `uidmap` package.
- At least 65536 subordinate IDs for your user in both `/etc/subuid` and `/etc/subgid`.
- `pasta` from the `passt` package for network access.
- Rust 1.85 or newer, Bash, `curl`, and `tar`.

On Debian or Ubuntu, install the system packages:

```bash
sudo apt install uidmap passt curl tar
```

## Get started

Run these commands in the project directory.

1. Install `aj`. Make sure `~/.cargo/bin` is in your `PATH`.

   ```bash
   cargo install --path . --locked
   ```

2. Download the container's base files (rootfs).

   ```bash
   ./scripts/fetch-rootfs.sh
   ```

3. Start a shell.

   ```bash
   aj run -- /bin/sh
   ```

Install any tools you need inside the container. They are removed when the run ends.

## Common options

| Option | Purpose |
| --- | --- |
| `-v HOST[:CONTAINER][:ro]` | Share a host file or directory. Add `:ro` for read-only access. |
| `-w /path` | Choose the working directory inside the container. |
| `--env KEY=VALUE` | Set an environment variable. Use `--env KEY` to copy a host value. |
| `--net none` | Block outside network access. |
| `--rootfs DIRECTORY` | Use another rootfs. |
| `--id ID` | Name the container. See [Check containers](#check-containers). |

Share the current directory:

```bash
aj run -v . -- /bin/sh
```

Without a container path, shared files use the same path as on the host.
Commands start in your current directory if it is shared, or in `/` otherwise.

Default limits: 1 CPU, 512 MiB of memory, and 1024 tasks (processes and threads).
Change them with `--cpus`, `--mem` (bytes), and `--pids`.
See `aj run --help` for all options.

## Environment and network

Only `TERM` and proxy variables (`http_proxy`, `https_proxy`, `no_proxy`, and their uppercase forms) are copied from the host.
Use `--env` for other variables.

Internet access is enabled by default through `pasta`. Without `pasta`, use `--net none`.
The container may reach host services, except those listening only on `127.0.0.1`.
The host cannot connect to servers inside the container.

## Check containers

Each container has an ID. `aj run` generates a random ID, or you can choose one with `--id`.
An ID has 1 to 128 letters, digits, `_`, `.`, and `-`, and starts with a letter or digit.
Two containers cannot use the same ID at the same time.

```bash
aj run --id build -- /bin/sh
```

In another terminal, list the containers:

```bash
aj list
```

Print the state of one container as OCI state JSON:

```bash
aj state build
```

The state of each container is stored in `$XDG_RUNTIME_DIR/agent-jail`. `aj run` removes it when the program ends.
Use `aj --root DIRECTORY` to store it in another directory.
If `aj` is killed, its container ends too.
`aj` runs as two processes: the runtime and the setup process.
If both are killed, for example with `pkill -9 aj`, `aj list` shows the container as `stopped`.
A program that changes its user, for example with `su`, can keep running after both processes are killed.
The next `aj run` or `aj create` ends such a program and removes what the killed run left behind.

## OCI bundles

Use `aj run-bundle DIRECTORY` to run an OCI bundle without outside network access.
Only some OCI 1.3.0 settings are supported. See the [example configuration](examples/oci-bundle/config.json).

As other OCI runtimes do, `aj` can also create the container first and start its program later:

```bash
aj create --bundle DIRECTORY build
aj start build
```

`aj create` returns when the container is ready. Without `--bundle`, it uses the current directory.
If you stop `aj create` with Ctrl-C before it returns, it removes the container.
The program receives the standard input and output of `aj create`, but it runs only after `aj start`.
After the program ends, the container stays as `stopped` and keeps its ID.
`aj` cannot delete such a container yet.

When you log out, systemd ends these containers, unless lingering is enabled for your user (`loginctl enable-linger`).

## Development

Use `make fmt` to format the code and `make lint` to check it with Clippy.

The tests in `tests/` start real containers, so `cargo test` skips them.
On a machine that meets the requirements, run them with `cargo test -- --ignored`.
