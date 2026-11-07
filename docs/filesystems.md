# Filesystem isolation

[Learning notes](README.md) · [Getting started](../README.md)

I decided to start with filesystem isolation. Filesystem isolation means that the container sees its own filesystem as the root of its environment — the host's filesystem is no longer visible from inside.

To achieve this, we need to leverage system calls that relocate and isolate the container's root directory. There are several ways to do it, and I'm going to implement two variants.

## Chroot-based filesystem isolation

This strategy performs filesystem isolation using the `chroot` system call.  
`chroot` changes the apparent root directory of the calling process (and its children) to a given path. Once applied, the process treats that path as its filesystem root — paths like `/` and `/etc` are resolved relative to it.

The implementation is as follows.

**1. Call `fork()` to create the container child process.**
```rust
pub fn run(args: RunArgs) -> Result<ExitCode> {
    // Make sure the rootfs exists before we fork.
    if !args.rootfs.is_dir() {
        // Exit early with a clear error message via the bail! macro.
        bail!("rootfs {:?} does not exist or is not a directory", args.rootfs);
    }

    // fork() is marked unsafe in `nix` because it cannot guarantee memory
    // safety across the parent/child split — we acknowledge that here.
    match unsafe { fork() }.context("fork failed")? {
        // Parent process
        ForkResult::Parent { child } => {
            // Wait for the child to finish and inspect its status.
            let status = waitpid(child, None).context("waitpid failed")?;
            match status {

                // Child exited normally — forward its exit code.
                WaitStatus::Exited(_, code) => Ok(ExitCode::from(code as u8)),

                // Child was killed by a signal (ctrl+c, kill, ...) —
                // return 128 + signal number (Linux convention).
                WaitStatus::Signaled(_, sig, _) => Ok(ExitCode::from(128u8 + sig as u8)),
                other => bail!("unexpected wait status: {:?}", other),
            }
        }

        // Child process (the container environment)
        ForkResult::Child => {

            // Run the child. If it fails, exit immediately with code 127
            // instead of returning into parent-side logic.
            if let Err(e) = child_main(args) {
                eprintln!("container-runtime: child failed: {e:#}");
                std::process::exit(127);
            }
            unreachable!("child_main either execs or exits");
        }
    }
}
```

**2. In the child process, call `chroot` to change its root directory to the given path.**
```rust
/// Restrict the process's view of the filesystem to `rootfs` using the `chroot` system call.
pub fn isolate_fs_chroot(rootfs: &Path) -> Result<()> {
    // Change the current process's root directory.
    chroot(rootfs).with_context(|| format!("chroot {:?}", rootfs))?;

    // Reset the working directory so it matches the new root.
    chdir("/").context("chdir(\"/\") after chroot")?;

    Ok(())
}
```

**3. After `chroot`, call `execvp` to replace the child's memory image with the target program (the container's entrypoint).**
```rust
/// Replace the current process with `cmd` + `args` via execvp.
/// Returns only on failure.
pub fn exec_cmd(cmd: &str, args: &[String]) -> Result<()> {
    // Convert the command into a nul-safe CString.
    let c_cmd = CString::new(cmd).context("cmd contains a nul byte")?;

    // Convert the arguments into nul-safe CStrings.
    let mut c_args: Vec<CString> = Vec::with_capacity(args.len() + 1);
    c_args.push(c_cmd.clone()); // argv[0] — conventionally the program name
    for arg in args {
        c_args.push(CString::new(arg.as_str()).context("arg contains a nul byte")?);
    }

    // Replace the current process image with the target program.
    execvp(&c_cmd, &c_args).with_context(|| format!("exec {:?}", cmd))?;

    unreachable!("failed to execute: {} {}", cmd, args.join(" "));
}
```

**Result**

As shown in the output below, the container's filesystem is successfully isolated from the host's.
```bash
root@0a77470d7094:/app# cargo run -- run ./rootfs /bin/sh -c 'cat /etc/os-release'
Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.03s
Running `target/debug/container-runtime run ./rootfs /bin/sh -c 'cat /etc/os-release'`
NAME="Alpine Linux"
ID=alpine
VERSION_ID=3.20.3
PRETTY_NAME="Alpine Linux v3.20"
HOME_URL="https://alpinelinux.org/"
BUG_REPORT_URL="https://gitlab.alpinelinux.org/alpine/aports/-/issues"
root@0a77470d7094:/app#
```

## chroot jailbreak

Modern container systems don't rely on `chroot` for filesystem isolation, because the root directory created by `chroot` can be escaped back to the host filesystem.
`chroot` only changes the starting point from which the process resolves absolute paths — it does not restrict anything else (open file descriptors, the current working directory, capabilities, namespaces, ...).

Let's walk through the classic `chroot` + `chdir` jailbreak scenario.

<img width="661" height="433" alt="Screenshot 2026-04-24 at 7 51 29 PM" src="https://github.com/user-attachments/assets/2364e00e-26d7-4d6e-8841-cc204f19def6" />

1. Inside the current chroot, create a new directory to chroot into again (e.g. `./escape`).  

2. Call `chroot("./escape")`.  

3. The process's apparent root is now `/rootfs/escape`, but its CWD is still `/rootfs` — which sits *outside* the new root.  

4. Repeatedly call `chdir("..")`.  

5. Because the CWD is already outside the new root, the kernel does not clamp `..` at the root boundary, so we keep climbing until we reach the host's real root directory.

6. Jailbreak success  

## pivot_root filesystem isolation

Because of the security problem above, modern container runtimes use the `pivot_root` system call to isolate the filesystem in a more secure way.

Unlike `chroot`, which only moves the starting point of absolute path resolution, `pivot_root` swaps the kernel's idea of the root mount itself, and when combined with a new mount namespace, makes mounts inside the container invisible to the host (and vice versa). The old root is not simply discarded. `pivot_root` moves it to another location, and we then unmount it to cut the last connection to the host filesystem.

The full sequence is:

<img width="481" height="645" alt="Screenshot 2026-04-24 at 7 57 37 PM" src="https://github.com/user-attachments/assets/d1e95aea-1f89-4a34-a9b9-bc9bcb708e85" />

1. `unshare(CLONE_NEWNS)`: create a new mount namespace.
2. `mount("/", MS_PRIVATE | MS_REC)`: stop mount events from propagating.
3. Bind-mount the rootfs onto itself: satisfy `pivot_root`'s mount-point requirement.
4. Open the old root and the new root as file descriptors.
5. `fchdir(new_root)` + `pivot_root(".", ".")`: swap the root mount without a directory for the old root.
6. `fchdir(old_root)` + `umount2(".", MNT_DETACH)`: detach the old root.
7. `chdir("/")`: move the working directory into the new root.
8. Mount a fresh `/proc` inside the container.

Each step deserves a closer look.

### 1. Create a new mount namespace

```rust
unshare(CloneFlags::CLONE_NEWNS).context("unshare(CLONE_NEWNS)")?;
```

`unshare(CLONE_NEWNS)` detaches this process's mount namespace from the host's. After this call, mount/unmount operations performed by this process can be confined to a separate view of the mount table.

Namespace separation alone is not sufficient, however. Linux's default mount propagation mode is `MS_SHARED`, which means mount events can still cross namespace boundaries. That is what the next step addresses.

### 2. Make "/" recursively private

```rust
mount::<str, _, str, str>(
    None,
    "/",
    None,
    MsFlags::MS_REC | MsFlags::MS_PRIVATE,
    None,
).context("mount / MS_REC|MS_PRIVATE")?;
```

This call does not create a new mount — it modifies the *propagation* property of the existing `/` mount, recursively applying `MS_PRIVATE` to every submount. Afterwards, any mount performed inside the container stays inside the container, and host-side mounts do not leak in.

The `mount()` syscall is overloaded: when `source` and `fstype` are both `None`, it is interpreted as "modify an existing mount" instead of "attach a new filesystem". The flag bits decide the exact semantics.

### 3. Bind-mount the rootfs onto itself

```rust
mount::<_, _, str, str>(
    Some(rootfs),
    rootfs,
    None,
    MsFlags::MS_BIND | MsFlags::MS_REC,
    None,
).with_context(|| format!("bind mount {:?} onto itself", rootfs))?;
```

`pivot_root` has a strict requirement: the new root must be a *mount point* distinct from its parent mount. A plain directory like `./rootfs` sitting on top of an existing mount does not qualify. Bind-mounting it onto itself creates an independent mount-table entry for the same files, which satisfies `pivot_root`.

`MS_REC` is added defensively — if there happen to be any submounts beneath the rootfs, we want them preserved in the new view.

### 4. Open both roots

```rust
let old_root = open_dir_path(Path::new("/"))?;
let new_root = open_dir_path(&self.rootfs)?;

fn open_dir_path(path: &Path) -> Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags((OFlag::O_PATH | OFlag::O_DIRECTORY).bits())
        .open(path)
        .with_context(|| format!("open {:?}", path))
}
```

After `pivot_root`, the path `/` refers to the new root. The process can then reach the old root only through a file descriptor that it opened before the call.

The new root must be opened **after** the bind mount in step 3. The kernel resolves the path again when we open it, and the last part of the path enters the bind mount. If we opened the rootfs before the bind mount, the descriptor would refer to the plain directory under the bind mount. That directory is not a mount point, so `pivot_root` would fail with `EINVAL`.

Two flags matter here:

- `O_PATH` opens the directory only to refer to it. The process does not need permission to read the directory.
- `O_CLOEXEC` closes the descriptor when the process calls `exec`. The Rust standard library sets it by default. Without it, the container program would inherit a descriptor that points to the host root.

`File` closes the descriptor when the value is dropped, so the function does not need explicit `close` calls.

### 5. Swap the root

```rust
fchdir(new_root.as_raw_fd()).context("fchdir to new root")?;
pivot_root(".", ".").context("pivot_root(\".\", \".\")")?;
```

`pivot_root(new_root, put_old)` moves the old root to `put_old`, which must be `new_root` itself or a directory below it. When both arguments are `"."`, the kernel mounts the old root on top of the new root at the same place. The `pivot_root(2)` man page describes this sequence in its NOTES section, and runc, youki, and crun use it.

Because no directory is needed for the old root, the runtime does not write to the rootfs. This matters for a read-only rootfs, and for a rootfs that container root cannot write to inside the user namespace.

After this call, `/` is the new root. The working directory, however, still refers to the old root, so the host filesystem is still reachable until step 6.

### 6. Detach the old root

```rust
fchdir(old_root.as_raw_fd()).context("fchdir to old root")?;
umount2(".", MntFlags::MNT_DETACH).context("detach old root")?;
```

With the current kernel, the working directory is already the old root after step 5. The kernel does not promise this, so we move there explicitly with the descriptor from step 4. runc, youki, and crun do the same.

`MNT_DETACH` is a *lazy* unmount. It disconnects the old root from the mount tree immediately, but the kernel releases it only after every process stops using it. A plain `umount` would fail with `EBUSY`, because our working directory is on the old root and the runtime binary and its shared libraries were loaded from the host filesystem.

There is no directory to remove after the unmount, because we never created one.

### 7. Reset the working directory

```rust
chdir("/").context("chdir(\"/\") after pivot_root")?;
```

After step 6, the working directory still refers to the detached old root. If we skip this step, any relative path used by the child process would resolve into the host filesystem. This is the same failure as the classic chroot escape.

### 8. Mount a fresh procfs

```rust
fs::create_dir_all("/proc").context("create /proc")?;
mount::<_, _, _, str>(
    Some("proc"),
    "/proc",
    Some("proc"),
    MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_NOEXEC,
    None,
).context("mount /proc")?;
```

Detaching the old root in step 6 also severed the container's view of `/proc`. Without it, `ps`, `top`, reads under `/proc/self/*`, and many shell built-ins stop working.

A few details are worth unpacking.

**`/proc` is not a regular directory.** It is a *virtual filesystem* synthesised by the kernel. Files like `/proc/cpuinfo` or `/proc/self/status` do not exist on any disk; the procfs driver fabricates their contents on each read. That is why `fs::create_dir_all("/proc")` alone is not sufficient — it creates an empty mount point, but no driver is attached to it.

**How does the kernel know to invoke procfs when something opens `/proc/...`?** The Linux VFS (Virtual Filesystem Switch) maintains a mount table that maps paths to filesystem drivers. When a process makes a path-based syscall, VFS walks the path and switches to the driver registered at whichever mount point it hits:

```
path prefix   →  driver
─────────────────────────
/             →  ext4
/proc         →  procfs
/sys          →  sysfs
/tmp          →  tmpfs
```

`mount()` is the act of inserting a row into that table. From that moment on, any access under the mount point is transparently routed to the driver — no user-level code needs to be aware of the dispatch.

**Why is `source` set to the literal string `"proc"`?** For an on-disk filesystem (`ext4`, `xfs`, ...), `source` points to a block device that the driver reads. For a virtual filesystem, there is no such device. The kernel does not use `source` at all. The string only shows up as a label in `mount` output and `/proc/self/mountinfo`. By convention, the source is set to the filesystem's type name (`proc`, `tmpfs`, `sysfs`), so `mount` output reads naturally as `proc on /proc type proc`. Tools that parse mount output rely on this convention.

**`MS_NOSUID | MS_NODEV | MS_NOEXEC`** are standard container hardening flags: they disable set-uid/set-gid binaries, device nodes, and executable files under `/proc`. Nothing in a healthy procfs needs any of them, so denying them closes a small attack surface.

**Result**

After all eight steps, the container's mount table contains only what we intentionally placed there:

```bash
/ # mount
/run/host_mark/Users on / type fakeowner (rw,nosuid,nodev,relatime,fakeowner)
proc on /proc type proc (rw,nosuid,nodev,noexec,relatime)
```

Mounts created inside the container do not leak to the host, and each new container launch starts from a fresh mount namespace:

```bash
# Container A
/ # mount -t tmpfs tmpfs /tmp
/ # mount | grep tmpfs
tmpfs on /tmp type tmpfs (rw,relatime)

# Container B (fresh launch)
/ # mount | grep tmpfs
/ #
```

Compared with the chroot escape demonstrated earlier, the retained-fd trick no longer works.
Because the detached old root is not reachable even via open file descriptors inherited from before the pivot.

```bash
cargo run -- run ./rootfs /bin/sh -c 'echo inside; ls /; mount | wc -l'
```

```text
root@6b7298085cd6:/app# cargo run -- run ./rootfs /bin/sh -c 'echo inside; ls /; mount | wc -l'
   Compiling container-runtime v0.1.0 (/app)
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.67s
     Running `target/debug/container-runtime run ./rootfs /bin/sh -c 'echo inside; ls /; mount | wc -l'`
inside
bin    dev    etc    home   lib    media  mnt    opt    proc   root   run    sbin   srv    sys    tmp    usr    var
2 (only root and /proc are mounted in new child process)
root@6b7298085cd6:/app# 
```

**Existing problem**

As we can see, we completed filesystem isolation but we didn't isolated the PID namespace.
If we run the container runtime and run `ps` command inside of it, we still can see the host's process.
```bash
PID   USER     TIME  COMMAND
    1 root      0:02 /bin/sh -c echo Container started trap "exit 0" 15  exec "$@" while sleep 1 & wait $!; do :; done -
   28 root      0:00 /bin/sh -c echo "New container started. Keep-alive process started." ; export VSCODE_REMOTE_CONTAINERS_SESSION=b96b2018-3691-4519-ad46-fadb534b58c31777019931519 ; /bin/sh
   34 root      0:00 /bin/sh
   40 root      0:00 /bin/sh
  214 root      0:00 /bin/sh
  216 root      0:00 sh /root/.vscode-server/bin/560a9dba96f961efea7b1612916f89e5d5d4d679/bin/code-server --log debug --force-disable-user-env --server-data-dir /root/.vscode-server --use-host-proxy --telemetry-level all --accept-server-license-terms --host 127.0.0.1 --port 0 --connection-token-
  230 root      0:23 /vscode/vscode-server/bin/linux-arm64/560a9dba96f961efea7b1612916f89e5d5d4d679/node /vscode/vscode-server/bin/linux-arm64/560a9dba96f961efea7b1612916f89e5d5d4d679/out/server-main.js --log debug --force-disable-user-env --server-data-dir /root/.vscode-server --use-host-proxy 
  243 root      0:00 /root/.vscode-server/bin/560a9dba96f961efea7b1612916f89e5d5d4d679/node /tmp/vscode-remote-containers-server-0aa04ef3-e653-42bd-b918-3889fca26696.js
  272 root      0:03 /root/.vscode-server/bin/560a9dba96f961efea7b1612916f89e5d5d4d679/node -e      const net = require('net');     const fs = require('fs');     process.stdin.pause();     const client = net.createConnection({ host: '127.0.0.1', port: 43011 }, () => {      console.error('Connect
  294 root      0:14 /root/.vscode-server/bin/560a9dba96f961efea7b1612916f89e5d5d4d679/node -e      const net = require('net');     const fs = require('fs');     process.stdin.pause();     const client = net.createConnection({ host: '127.0.0.1', port: 43011 }, () => {      console.error('Connect
  314 root      0:04 /vscode/vscode-server/bin/linux-arm64/560a9dba96f961efea7b1612916f89e5d5d4d679/node /vscode/vscode-server/bin/linux-arm64/560a9dba96f961efea7b1612916f89e5d5d4d679/out/bootstrap-fork --type=fileWatcher
  326 root      2:31 /vscode/vscode-server/bin/linux-arm64/560a9dba96f961efea7b1612916f89e5d5d4d679/node --dns-result-order=ipv4first /vscode/vscode-server/bin/linux-arm64/560a9dba96f961efea7b1612916f89e5d5d4d679/out/bootstrap-fork --type=extensionHost --transformURIs --useHostProxy=true
  383 root      0:05 /vscode/vscode-server/bin/linux-arm64/560a9dba96f961efea7b1612916f89e5d5d4d679/node /vscode/vscode-server/bin/linux-arm64/560a9dba96f961efea7b1612916f89e5d5d4d679/out/bootstrap-fork --type=ptyHost --logsPath /root/.vscode-server/data/logs/20260424T083904
  414 root      0:01 /vscode/vscode-server/bin/linux-arm64/560a9dba96f961efea7b1612916f89e5d5d4d679/node /vscode/vscode-server/bin/linux-arm64/560a9dba96f961efea7b1612916f89e5d5d4d679/extensions/json-language-features/server/dist/node/jsonServerMain --node-ipc --clientProcessId=326
  552 root      3:57 /root/.vscode-server/extensions/rust-lang.rust-analyzer-0.3.2870-linux-arm64/server/rust-analyzer
  855 root      0:00 /usr/local/rustup/toolchains/1.95.0-aarch64-unknown-linux-gnu/libexec/rust-analyzer-proc-macro-srv
 3101 root      0:04 /vscode/vscode-server/bin/linux-arm64/560a9dba96f961efea7b1612916f89e5d5d4d679/node /vscode/vscode-server/bin/linux-arm64/560a9dba96f961efea7b1612916f89e5d5d4d679/extensions/markdown-language-features/dist/serverWorkerMain --node-ipc --clientProcessId=326
58211 root      0:00 /bin/bash --init-file /vscode/vscode-server/bin/linux-arm64/560a9dba96f961efea7b1612916f89e5d5d4d679/out/vs/workbench/contrib/terminal/common/scripts/shellIntegration-bash.sh
59413 root      0:00 target/debug/container-runtime run ./rootfs /bin/sh
59423 root      0:00 /bin/sh
59432 root      0:00 /bin/sh -c "/vscode/vscode-server/bin/linux-arm64/560a9dba96f961efea7b1612916f89e5d5d4d679/out/vs/base/node/cpuUsage.sh" 58211 59413 59423
59433 root      0:00 {cpuUsage.sh} /bin/bash /vscode/vscode-server/bin/linux-arm64/560a9dba96f961efea7b1612916f89e5d5d4d679/out/vs/base/node/cpuUsage.sh 58211 59413 59423
59438 root      0:00 sleep 1
59439 root      0:00 sleep 1
59440 root      0:00 ps
```
