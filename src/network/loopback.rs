use anyhow::{Context, Result};
use nix::errno::Errno;
use nix::libc;
use nix::sys::socket::{AddressFamily, SockFlag, SockType, socket};
use std::os::fd::AsRawFd;

/// Set the `lo` interface of the current network namespace up.
///
/// A new network namespace starts with `lo` down, so even connections to `localhost` fail.
/// The caller needs `CAP_NET_ADMIN` in the user namespace that owns the network namespace.
pub fn bring_up_loopback() -> Result<()> {
    // Interface ioctls work on any socket. The socket only selects the network namespace.
    let socket = socket(
        AddressFamily::Inet,
        SockType::Datagram,
        SockFlag::SOCK_CLOEXEC,
        None,
    )
    .context("open a socket for interface ioctls")?;

    // SAFETY: ifreq is a plain C struct, and all zero bytes are a valid value.
    let mut request: libc::ifreq = unsafe { std::mem::zeroed() };
    for (target, &byte) in request.ifr_name.iter_mut().zip(b"lo\0") {
        *target = byte as libc::c_char;
    }

    // Read the current flags first, because SIOCSIFFLAGS replaces all of them.
    // SAFETY: the request is a valid ifreq with a nul-terminated name.
    Errno::result(unsafe { libc::ioctl(socket.as_raw_fd(), libc::SIOCGIFFLAGS, &mut request) })
        .context("read the flags of lo")?;
    // SAFETY: SIOCGIFFLAGS filled ifru_flags.
    unsafe { request.ifr_ifru.ifru_flags |= libc::IFF_UP as libc::c_short };
    // SAFETY: the request is still a valid ifreq.
    Errno::result(unsafe { libc::ioctl(socket.as_raw_fd(), libc::SIOCSIFFLAGS, &request) })
        .context("set lo up")?;
    Ok(())
}
