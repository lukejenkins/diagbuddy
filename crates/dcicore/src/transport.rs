// SPDX-License-Identifier: Apache-2.0
//! Two ways to reach the same DCI interface. Nothing above this module knows which one is in use.
//!
//! * **Kernel**: `/dev/diag`, requests are ioctls, subscription and data are write/read.
//! * **Router**: the userspace diag-router's abstract `SOCK_SEQPACKET` socket. A request is a
//!   message `[u32 op][kernel struct]`; replies are tagged in their first `u32`
//!   (`0x10000` hello, `0x20000` client id, `0x40000` support list). Subscription and data
//!   messages are byte-for-byte the kernel's write and read.

use crate::dci::{router_register_body, OP_DEINIT, OP_REGISTER, OP_SUPPORT, REGISTER_LEN, SUPPORT_LEN};
use crate::sys;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::io::{AsRawFd, FromRawFd};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

pub const KERNEL_DEV: &str = "/dev/diag";

const ROUTER_HELLO: u32 = 0x33;
const TAG_HELLO: u32 = 0x10000;
const TAG_REGISTER: u32 = 0x20000;
const TAG_SUPPORT: u32 = 0x40000;
/// The whole router handshake (hello, register, support) must finish in this long. Every
/// read is bounded by what is left of it, so a router that keeps sending other traffic cannot
/// stretch the wait read by read. The caller is the daemon's supervisor thread, which also
/// writes the status file and answers SIGTERM; before, each awaited reply could take
/// 12 reads x 3 s.
pub const ROUTER_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
/// A write to the router that cannot complete in this long fails (a router that has stopped
/// reading must cost a lost session, not a caller blocked for ever). Set in the handshake and
/// left set. A SEQPACKET write is all or nothing, so a timed-out one sent nothing.
pub const ROUTER_WRITE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Which {
    Auto,
    Kernel,
    Router,
}

pub enum Transport {
    Kernel(File),
    Router(UnixStream),
}

/// Result of the start-up handshake.
#[derive(Debug, Clone, Copy)]
pub struct Session {
    pub client_id: i32,
    /// Bitmask of processors that support DCI, as the stack reports it.
    pub support: u16,
}

fn bad(msg: String) -> io::Error {
    io::Error::other(msg)
}

impl Transport {
    pub fn open(which: Which) -> io::Result<Transport> {
        let kernel = || OpenOptions::new().read(true).write(true).open(KERNEL_DEV).map(Transport::Kernel);
        let router = || Transport::open_router_at(&sys::ROUTER_NAME);
        match which {
            Which::Kernel => kernel(),
            Which::Router => router(),
            Which::Auto => kernel().or_else(|ke| {
                router().map_err(|re| bad(format!("no {KERNEL_DEV} ({ke}) and no diag-router socket ({re})")))
            }),
        }
    }

    /// Connect to the router socket at the abstract address `name` (the bytes after the
    /// namespace NUL; the real router's is [`sys::ROUTER_NAME`]). Tests use a unique name.
    pub fn open_router_at(name: &[u8]) -> io::Result<Transport> {
        let fd = sys::connect_router_at(name)?;
        // A SEQPACKET socket used through UnixStream: only read/write/setsockopt are called.
        Ok(Transport::Router(unsafe { UnixStream::from_raw_fd(fd) }))
    }

    pub fn name(&self) -> &'static str {
        match self {
            Transport::Kernel(_) => "kernel /dev/diag",
            Transport::Router(_) => "diag-router socket",
        }
    }

    /// Support query and registration, in the order each transport was measured to accept.
    pub fn handshake(&mut self) -> io::Result<Session> {
        self.handshake_within(ROUTER_HANDSHAKE_TIMEOUT)
    }

    /// [`Transport::handshake`] with an explicit deadline for the router's replies. The
    /// kernel transport's ioctls do not wait on a peer, so it ignores the deadline.
    pub fn handshake_within(&mut self, limit: Duration) -> io::Result<Session> {
        let deadline = Instant::now() + limit;
        let s = match self {
            Transport::Kernel(f) => {
                let fd = f.as_raw_fd();
                let mut sup = [0u8; SUPPORT_LEN];
                // SAFETY: `sup` is the driver's SUPPORT_LEN-byte support-query struct.
                unsafe { sys::ioctl_ptr(fd, OP_SUPPORT, sup.as_mut_ptr())? };
                let mut reg = [0u8; REGISTER_LEN];
                // SAFETY: `reg` is the driver's REGISTER_LEN-byte register struct.
                let client_id = unsafe { sys::ioctl_ptr(fd, OP_REGISTER, reg.as_mut_ptr())? };
                Session { client_id, support: u16::from_le_bytes([sup[4], sup[5]]) }
            }
            Transport::Router(s) => {
                s.set_write_timeout(Some(ROUTER_WRITE_TIMEOUT))?;
                s.write_all(&ROUTER_HELLO.to_le_bytes())?;
                router_reply(s, TAG_HELLO, deadline)?;
                s.write_all(&request(OP_REGISTER, &router_register_body(std::process::id())))?;
                let r = router_reply(s, TAG_REGISTER, deadline)?;
                let client_id = r
                    .get(..4)
                    .map(|b| i32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                    .ok_or_else(|| bad("short register reply".into()))?;
                // From here the router holds a client for this pid. A failure before the
                // session is handed back must say so, or the entry is left in its table.
                let rest = (|| {
                    s.write_all(&request(OP_SUPPORT, &[0u8; SUPPORT_LEN]))?;
                    let sup = router_reply(s, TAG_SUPPORT, deadline)?;
                    s.set_read_timeout(None)?;
                    Ok::<_, io::Error>(sup)
                })();
                let sup = match rest {
                    Ok(sup) => sup,
                    Err(e) => {
                        if valid_client_id(client_id) {
                            // Best effort, as on a normal teardown: no reply is awaited.
                            let _ = s.write_all(&request(OP_DEINIT, &client_id.to_le_bytes()));
                        }
                        return Err(e);
                    }
                };
                Session {
                    client_id,
                    support: sup.get(..2).map(|b| u16::from_le_bytes([b[0], b[1]])).unwrap_or(0),
                }
            }
        };
        if !valid_client_id(s.client_id) {
            return Err(bad(format!("registration failed: client id {}", s.client_id)));
        }
        Ok(s)
    }

    /// Send a subscription message (same bytes on both transports).
    pub fn send(&mut self, msg: &[u8]) -> io::Result<()> {
        match self {
            // One write(): /dev/diag answers a DCI write with a status, not a byte count, so
            // write_all() would slice past the message and panic.
            Transport::Kernel(f) => kernel_write_result(msg.len(), f.write(msg)?),
            Transport::Router(s) => s.write_all(msg),
        }
    }

    pub fn deregister(&mut self, client_id: i32) -> io::Result<()> {
        match self {
            Transport::Kernel(f) => {
                let mut c = client_id;
                // SAFETY: the deinit ioctl takes a pointer to one `i32` client id.
                unsafe { sys::ioctl_ptr(f.as_raw_fd(), OP_DEINIT, &mut c as *mut i32 as *mut u8) }
                    .map(|_| ())
            }
            // `[21][client id]`, as the vendor client sends it (FX4200 trace). No reply
            // is awaited; the vendor client reads none before closing.
            Transport::Router(s) => s.write_all(&request(OP_DEINIT, &client_id.to_le_bytes())),
        }
    }

    /// A second handle for the read thread.
    pub fn reader(&self) -> io::Result<Box<dyn Read + Send>> {
        Ok(match self {
            Transport::Kernel(f) => Box::new(f.try_clone()?),
            Transport::Router(s) => Box::new(s.try_clone()?),
        })
    }
}

/// Judge a `/dev/diag` write's return. Measured on an FX2000 (SDX55): a DCI subscribe
/// write returns 1001, not its length. A full byte count is also accepted, so a
/// driver that does return lengths keeps working. Anything else is reported, never sliced on.
const KERNEL_DCI_WRITE_OK: usize = 1001;

fn kernel_write_result(len: usize, ret: usize) -> io::Result<()> {
    if ret == len || ret == KERNEL_DCI_WRITE_OK {
        Ok(())
    } else {
        Err(bad(format!("/dev/diag DCI write of {len} B returned {ret}")))
    }
}

/// The probe saw ids 1..; anything else means registration did not take.
fn valid_client_id(id: i32) -> bool {
    (1..=1000).contains(&id)
}

fn request(op: u32, body: &[u8]) -> Vec<u8> {
    let mut m = op.to_le_bytes().to_vec();
    m.extend_from_slice(body);
    m
}

fn router_reply(s: &mut UnixStream, want: u32, deadline: Instant) -> io::Result<Vec<u8>> {
    let mut buf = vec![0u8; 65536];
    let none = || bad(format!("diag-router sent no 0x{want:x} reply in time"));
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(none());
        }
        s.set_read_timeout(Some(left))?;
        let n = match s.read(&mut buf) {
            Ok(n) => n,
            Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {
                return Err(none())
            }
            Err(e) => return Err(e),
        };
        if n == 0 {
            return Err(bad(format!("diag-router closed the socket before its 0x{want:x} reply")));
        }
        if n >= 4 && u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) == want {
            return Ok(buf[4..n].to_vec());
        }
    }
}

#[cfg(test)]
mod kernel_write_tests {
    use super::*;

    #[test]
    fn the_measured_dci_status_return_is_success() {
        // An FX2000 (SDX55, /dev/diag): a 32-byte subscribe write returned 1001.
        assert!(kernel_write_result(32, 1001).is_ok());
    }

    #[test]
    fn a_full_byte_count_is_success() {
        assert!(kernel_write_result(32, 32).is_ok());
    }

    #[test]
    fn any_other_return_is_an_error_naming_the_value() {
        for ret in [0usize, 16, 1000, 1002] {
            let e = kernel_write_result(32, ret).unwrap_err();
            assert!(e.to_string().contains(&ret.to_string()), "{e}");
        }
    }
}
