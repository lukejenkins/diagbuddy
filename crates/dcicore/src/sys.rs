// SPDX-License-Identifier: Apache-2.0
//! The four libc calls dcicore needs, declared by hand so the crate stays `std`-only.

use std::io;
use std::os::unix::io::{AsRawFd, FromRawFd};
use std::os::unix::net::UnixStream;
use std::time::Duration;
use std::os::raw::{c_int, c_ulong, c_void};
use std::sync::atomic::{AtomicBool, Ordering};

extern "C" {
    fn ioctl(fd: c_int, req: c_ulong, ...) -> c_int;
    fn socket(domain: c_int, ty: c_int, proto: c_int) -> c_int;
    fn connect(fd: c_int, addr: *const c_void, len: u32) -> c_int;
    fn signal(sig: c_int, handler: usize) -> usize;
}

const AF_UNIX: c_int = 1;
const SOCK_SEQPACKET: c_int = 5;
const SOCK_CLOEXEC: c_int = 0o2000000;

/// `ioctl(fd, req, arg)`. Returns the call's value (the DCI register call returns the client id).
///
/// # Safety
///
/// `arg` must point to writable memory at least as large as the struct the driver reads and
/// writes for `req`, and stay valid for the whole call.
pub unsafe fn ioctl_ptr(fd: c_int, req: u32, arg: *mut u8) -> io::Result<c_int> {
    // SAFETY: the caller guarantees `arg` is valid for `req`.
    let r = unsafe { ioctl(fd, req as c_ulong, arg) };
    if r < 0 { Err(io::Error::last_os_error()) } else { Ok(r) }
}

/// The userspace diag-router's abstract address: a `sun_path` of 108 NUL bytes, which is a
/// leading NUL (marking the abstract namespace) and a 107-byte all-zero name (observed on the
/// FX3100).
pub const ROUTER_NAME: [u8; 107] = [0u8; 107];

/// Longest abstract name: `sun_path` is 108 bytes and the first is the namespace NUL.
pub const MAX_ABSTRACT_NAME: usize = 107;

/// The `sockaddr_un` bytes for the abstract address `name`: `sa_family_t`, a NUL, then `name`.
/// Its length is the address length passed to `connect`/`bind`, so the name is exact (no
/// zero padding beyond what `name` itself holds).
pub fn abstract_sockaddr(name: &[u8]) -> io::Result<Vec<u8>> {
    if name.len() > MAX_ABSTRACT_NAME {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "abstract socket name over 107 bytes"));
    }
    let mut addr = Vec::with_capacity(3 + name.len());
    addr.extend_from_slice(&(AF_UNIX as u16).to_ne_bytes());
    addr.push(0);
    addr.extend_from_slice(name);
    Ok(addr)
}

/// How long a connect to the router may wait. A connect to a listener whose backlog is full
/// sleeps until the listener accepts, for as long as the socket's send timeout allows, so
/// without one a stuck router blocks the caller for ever.
pub const ROUTER_CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

/// Connect a `SOCK_SEQPACKET` socket to the abstract AF_UNIX address `name` (the bytes after
/// the namespace NUL), waiting at most [`ROUTER_CONNECT_TIMEOUT`]. The real router is [`ROUTER_NAME`].
pub fn connect_router_at(name: &[u8]) -> io::Result<c_int> {
    connect_router_within(name, ROUTER_CONNECT_TIMEOUT).map(|s| {
        use std::os::unix::io::IntoRawFd;
        s.into_raw_fd()
    })
}

/// [`connect_router_at`] with an explicit connect timeout. The send timeout is set before
/// `connect` (Linux AF_UNIX connect waits on it) and left set; the handshake sets its own.
pub fn connect_router_within(name: &[u8], timeout: Duration) -> io::Result<UnixStream> {
    let addr = abstract_sockaddr(name)?;
    let fd = unsafe { socket(AF_UNIX, SOCK_SEQPACKET | SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // Owned from here, so every early return closes it. A SEQPACKET socket used through
    // UnixStream: only setsockopt, connect, read and write are called on it. std sets the
    // timeout portably (struct timeval differs between 32- and 64-bit musl).
    let s = unsafe { UnixStream::from_raw_fd(fd) };
    s.set_write_timeout(Some(timeout))?;
    let r = unsafe { connect(s.as_raw_fd(), addr.as_ptr() as *const c_void, addr.len() as u32) };
    if r < 0 {
        let e = io::Error::last_os_error();
        return Err(if e.kind() == io::ErrorKind::WouldBlock {
            io::Error::new(io::ErrorKind::TimedOut, format!("diag-router connect: no accept in {timeout:?}"))
        } else {
            e
        });
    }
    Ok(s)
}

pub static STOP: AtomicBool = AtomicBool::new(false);
pub static DUMP_STATS: AtomicBool = AtomicBool::new(false);
/// Set by SIGUSR2: a debug request to drop the DCI session and register again in-process.
pub static REREGISTER: AtomicBool = AtomicBool::new(false);

const SIGHUP: c_int = 1;
const SIGINT: c_int = 2;
const SIGUSR1: c_int = 10; // same number on x86, x86_64, arm and aarch64 Linux
const SIGUSR2: c_int = 12; // likewise
const SIGTERM: c_int = 15;
const SIG_IGN: usize = 1;

extern "C" fn on_signal(sig: c_int) {
    if sig == SIGUSR1 {
        DUMP_STATS.store(true, Ordering::Relaxed);
    } else if sig == SIGUSR2 {
        REREGISTER.store(true, Ordering::Relaxed);
    } else {
        STOP.store(true, Ordering::Relaxed);
    }
}

/// SIGTERM / SIGINT request a clean stop (deregister first); SIGUSR1 asks for a stats line;
/// SIGUSR2 asks for an in-process re-registration (a debug aid; see [`REREGISTER`]).
/// SIGHUP is ignored: started with `&` from `adb shell`, dcitap got SIGHUP when that shell
/// exited and died by the default action, with no deregister and no file flush (an FX3100). A tap meant to run beside rayhunter should outlive its shell.
pub fn install_signal_handlers() {
    let h = on_signal as extern "C" fn(c_int) as usize;
    unsafe {
        signal(SIGHUP, SIG_IGN);
        signal(SIGINT, h);
        signal(SIGTERM, h);
        signal(SIGUSR1, h);
        signal(SIGUSR2, h);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_router_address_is_the_original_110_zero_bytes_apart_from_the_family() {
        let a = abstract_sockaddr(&ROUTER_NAME).unwrap();
        assert_eq!(a.len(), 110);
        assert_eq!(&a[..2], &(AF_UNIX as u16).to_ne_bytes());
        assert!(a[2..].iter().all(|&b| b == 0));
    }

    #[test]
    fn a_named_address_is_family_nul_name_and_an_overlong_one_is_refused() {
        let a = abstract_sockaddr(b"abc").unwrap();
        assert_eq!(&a[2..], &[0, b'a', b'b', b'c']);
        assert!(abstract_sockaddr(&[1u8; 108]).is_err());
    }

    extern "C" {
        fn raise(sig: c_int) -> c_int;
    }

    #[test]
    fn sighup_is_ignored_and_does_not_stop_or_kill() {
        install_signal_handlers();
        // signal() returns the previous disposition: it must be SIG_IGN, not the stop handler.
        let prev = unsafe { signal(SIGHUP, SIG_IGN) };
        assert_eq!(prev, SIG_IGN, "SIGHUP disposition after install is {prev:#x}, not SIG_IGN");
        // Delivered for real: the default action would kill this test process outright.
        assert_eq!(unsafe { raise(SIGHUP) }, 0);
        assert!(!STOP.load(Ordering::Relaxed), "SIGHUP must not request a stop");
    }

    #[test]
    fn sigusr2_requests_a_reregistration_and_not_a_stop() {
        install_signal_handlers();
        REREGISTER.store(false, Ordering::Relaxed);
        // Delivered for real: the default action would kill this test process outright.
        assert_eq!(unsafe { raise(SIGUSR2) }, 0);
        assert!(REREGISTER.swap(false, Ordering::Relaxed), "SIGUSR2 must set REREGISTER");
        assert!(!STOP.load(Ordering::Relaxed), "SIGUSR2 must not request a stop");
    }
}
