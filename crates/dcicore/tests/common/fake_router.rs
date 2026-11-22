// SPDX-License-Identifier: Apache-2.0
//! A fake userspace diag-router for tests: an abstract `SOCK_SEQPACKET` listener that speaks
//! the DCI handshake and data-read layout `dcicore` expects.
//!
//! Self-contained (std, hand-declared libc, and `dcicore`'s public constants) so another
//! crate's integration tests can pull it in with `#[path = ".../fake_router.rs"] mod fake_router;`.
//!
//! Determinism: [`FakeRouter::push_logs`], [`FakeRouter::subscriptions`],
//! [`FakeRouter::registrations`] and [`FakeRouter::drop_client`] first wait until every message
//! the client has already sent has been processed. A message handler only consumes a message
//! while holding the state lock, and the waiting side checks the socket's unread byte count
//! under that same lock, so "queue empty" means "all sent messages are recorded".
//! [`FakeRouter::stop_reading`] turns that off: from then on nothing is consumed, so the
//! client's writes back up until they block, and the waiting side no longer waits.
#![allow(dead_code)]

use dcicore::dci::{DCI_DATA, OP_DEINIT, OP_REGISTER, OP_SUPPORT, REC_LOG};
use std::io::Write;
use std::os::raw::{c_int, c_ulong, c_void};
use std::os::unix::io::{AsRawFd, FromRawFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

#[repr(C)]
struct PollFd {
    fd: c_int,
    events: i16,
    revents: i16,
}

extern "C" {
    fn socket(domain: c_int, ty: c_int, proto: c_int) -> c_int;
    fn bind(fd: c_int, addr: *const c_void, len: u32) -> c_int;
    fn listen(fd: c_int, backlog: c_int) -> c_int;
    fn accept4(fd: c_int, addr: *mut c_void, len: *mut u32, flags: c_int) -> c_int;
    fn poll(fds: *mut PollFd, n: c_ulong, timeout_ms: c_int) -> c_int;
    fn recv(fd: c_int, buf: *mut c_void, len: usize, flags: c_int) -> isize;
    fn shutdown(fd: c_int, how: c_int) -> c_int;
    fn close(fd: c_int) -> c_int;
    fn ioctl(fd: c_int, req: c_ulong, ...) -> c_int;
}

const AF_UNIX: c_int = 1;
const SOCK_SEQPACKET: c_int = 5;
const SOCK_CLOEXEC: c_int = 0o2000000;
const MSG_PEEK: c_int = 2;
const MSG_DONTWAIT: c_int = 0x40;
const SHUT_RDWR: c_int = 2;
const POLLIN: i16 = 1;
const FIONREAD: c_ulong = 0x541B;

const TAG_HELLO: u32 = 0x10000;
const TAG_REGISTER: u32 = 0x20000;
const TAG_SUPPORT: u32 = 0x40000;
const HELLO: u32 = 0x33;
const WAIT_LIMIT: Duration = Duration::from_secs(5);

#[derive(Default)]
struct State {
    registrations: Vec<[u8; 20]>,
    subscriptions: Vec<(bool, Vec<u16>)>,
    /// Client ids from `OP_DEINIT` messages.
    deregistrations: Vec<i32>,
    client: Option<Arc<UnixStream>>,
    /// Connection counter, so a finished handler only clears its own connection.
    conn_id: u64,
    ts: u64,
}

struct Shared {
    state: Mutex<State>,
    stop: AtomicBool,
    /// Consume nothing more from any connection (a router that has stopped reading).
    deaf: AtomicBool,
    /// Leave `OP_SUPPORT` unanswered (a handshake that fails after the register reply).
    mute_support: AtomicBool,
    /// Non-zero: answer nothing, and send the client an untagged message every this many ms
    /// (a router busy with other traffic, which never gets to the handshake).
    chatter_ms: AtomicU64,
}

/// An abstract socket name unique to this test process, so two `cargo test` runs on one
/// host never bind the same name.
pub fn unique(base: &str) -> Vec<u8> {
    format!("{base}-{}", std::process::id()).into_bytes()
}

pub struct FakeRouter {
    shared: Arc<Shared>,
    listen_fd: c_int,
    accept_thread: Option<JoinHandle<()>>,
    chatter_thread: Option<JoinHandle<()>>,
}

fn le32(b: &[u8], o: usize) -> Option<u32> {
    b.get(o..o + 4).map(|x| u32::from_le_bytes([x[0], x[1], x[2], x[3]]))
}

fn reply(conn: &UnixStream, tag: u32, body: &[u8]) {
    let mut m = tag.to_le_bytes().to_vec();
    m.extend_from_slice(body);
    let _ = (&*conn).write_all(&m);
}

/// Bytes the client has sent that nobody has read yet (0 on any error).
fn unread(fd: c_int) -> usize {
    let mut n: c_int = 0;
    let r = unsafe { ioctl(fd, FIONREAD, &mut n as *mut c_int) };
    if r < 0 { 0 } else { n as usize }
}

fn handle(st: &mut State, conn: &UnixStream, msg: &[u8], mute_support: bool) {
    if msg.len() == 4 && le32(msg, 0) == Some(HELLO) {
        return reply(conn, TAG_HELLO, &[]);
    }
    match le32(msg, 0) {
        Some(OP_REGISTER) => {
            let mut reg = [0u8; 20];
            let body = &msg[4..];
            let n = body.len().min(20);
            reg[..n].copy_from_slice(&body[..n]);
            st.registrations.push(reg);
            reply(conn, TAG_REGISTER, &1i32.to_le_bytes());
        }
        Some(OP_SUPPORT) if mute_support => {}
        Some(OP_SUPPORT) => reply(conn, TAG_SUPPORT, &[0x03, 0x00]),
        Some(OP_DEINIT) => {
            if let Some(id) = le32(msg, 4) {
                st.deregistrations.push(id as i32);
            }
        }
        // `[0x40][-1][client][set][n][u16 code; n]`
        Some(v) if v as i32 == DCI_DATA => {
            let set = le32(msg, 12).map(|v| v != 0);
            let n = le32(msg, 16).map(|v| v as usize);
            if let (Some(set), Some(n)) = (set, n) {
                let codes = (0..n)
                    .filter_map(|i| msg.get(20 + 2 * i..22 + 2 * i))
                    .map(|c| u16::from_le_bytes([c[0], c[1]]))
                    .collect();
                st.subscriptions.push((set, codes));
            }
        }
        _ => {}
    }
}

fn serve(shared: Arc<Shared>, conn: Arc<UnixStream>, id: u64) {
    let fd = conn.as_raw_fd();
    let mut buf = vec![0u8; 65536];
    loop {
        // Wait for a message without consuming it; the real read happens under the lock.
        let mut one = [0u8; 1];
        let n = unsafe { recv(fd, one.as_mut_ptr() as *mut c_void, 1, MSG_PEEK) };
        if n < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
            continue;
        }
        if n <= 0 {
            break; // closed, shut down, or failed
        }
        if shared.deaf.load(Ordering::Relaxed) {
            // Leave the message unread; a shut-down socket still peeks its queued data.
            if shared.stop.load(Ordering::Relaxed) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
            continue;
        }
        let mut st = shared.state.lock().unwrap();
        if shared.deaf.load(Ordering::Relaxed) {
            continue; // stop_reading ran while this thread waited for the lock
        }
        let n = unsafe { recv(fd, buf.as_mut_ptr() as *mut c_void, buf.len(), MSG_DONTWAIT) };
        if n > 0 && shared.chatter_ms.load(Ordering::Relaxed) == 0 {
            handle(&mut st, &conn, &buf[..n as usize], shared.mute_support.load(Ordering::Relaxed));
        }
    }
    let mut st = shared.state.lock().unwrap();
    if st.conn_id == id {
        st.client = None;
    }
}

fn chatter_loop(shared: Arc<Shared>) {
    while !shared.stop.load(Ordering::Relaxed) {
        let ms = shared.chatter_ms.load(Ordering::Relaxed);
        if ms == 0 {
            std::thread::sleep(Duration::from_millis(5));
            continue;
        }
        let conn = shared.state.lock().unwrap().client.clone();
        if let Some(c) = conn {
            reply(&c, 0xDEAD_0000, &[0u8; 8]);
        }
        std::thread::sleep(Duration::from_millis(ms));
    }
}

fn accept_loop(shared: Arc<Shared>, listen_fd: c_int) {
    let mut conns: Vec<(Arc<UnixStream>, JoinHandle<()>)> = Vec::new();
    while !shared.stop.load(Ordering::Relaxed) {
        let mut p = PollFd { fd: listen_fd, events: POLLIN, revents: 0 };
        if unsafe { poll(&mut p, 1, 20) } <= 0 || p.revents & POLLIN == 0 {
            continue;
        }
        let fd = unsafe { accept4(listen_fd, std::ptr::null_mut(), std::ptr::null_mut(), SOCK_CLOEXEC) };
        if fd < 0 {
            continue;
        }
        let conn = Arc::new(unsafe { UnixStream::from_raw_fd(fd) });
        let id = {
            let mut st = shared.state.lock().unwrap();
            st.conn_id += 1;
            st.client = Some(conn.clone());
            st.conn_id
        };
        let (s, c) = (shared.clone(), conn.clone());
        conns.push((conn, std::thread::spawn(move || serve(s, c, id))));
    }
    for (conn, h) in conns {
        unsafe { shutdown(conn.as_raw_fd(), SHUT_RDWR) };
        let _ = h.join();
    }
}

impl FakeRouter {
    /// Listen on the abstract address `name` (the bytes after the namespace NUL, as for
    /// `Transport::open_router_at`). Panics if the name is taken.
    pub fn start(name: &[u8]) -> FakeRouter {
        assert!(name.len() <= 107, "abstract name too long");
        let mut addr = (AF_UNIX as u16).to_ne_bytes().to_vec();
        addr.push(0);
        addr.extend_from_slice(name);
        let fd = unsafe { socket(AF_UNIX, SOCK_SEQPACKET | SOCK_CLOEXEC, 0) };
        assert!(fd >= 0, "socket: {}", std::io::Error::last_os_error());
        let ok = unsafe { bind(fd, addr.as_ptr() as *const c_void, addr.len() as u32) } == 0
            && unsafe { listen(fd, 8) } == 0;
        if !ok {
            let e = std::io::Error::last_os_error();
            unsafe { close(fd) };
            panic!("FakeRouter bind/listen on {:?}: {e}", String::from_utf8_lossy(name));
        }
        let shared = Arc::new(Shared {
            state: Mutex::new(State::default()),
            stop: AtomicBool::new(false),
            deaf: AtomicBool::new(false),
            mute_support: AtomicBool::new(false),
            chatter_ms: AtomicU64::new(0),
        });
        let s = shared.clone();
        let accept_thread = Some(std::thread::spawn(move || accept_loop(s, fd)));
        let s = shared.clone();
        let chatter_thread = Some(std::thread::spawn(move || chatter_loop(s)));
        FakeRouter { shared, listen_fd: fd, accept_thread, chatter_thread }
    }

    /// The state lock, taken only once the client's sent messages have all been processed.
    /// After [`FakeRouter::stop_reading`], unread messages stay unread and this does not wait.
    fn quiesced(&self) -> MutexGuard<'_, State> {
        let deadline = Instant::now() + WAIT_LIMIT;
        loop {
            let st = self.shared.state.lock().unwrap();
            let pending = st.client.as_ref().map(|c| unread(c.as_raw_fd())).unwrap_or(0);
            if pending == 0 || self.shared.deaf.load(Ordering::Relaxed) {
                return st;
            }
            drop(st);
            assert!(Instant::now() < deadline, "FakeRouter: client messages were not processed in time");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// Send the client one data read holding these log records, after processing everything
    /// the client has already sent. Panics if no client is connected.
    pub fn push_logs(&self, logs: &[(u16, Vec<u8>)]) {
        let mut st = self.quiesced();
        let conn = st.client.clone().expect("push_logs: no client connected");
        let mut body = Vec::new();
        for (code, payload) in logs {
            st.ts += 1;
            body.extend_from_slice(&REC_LOG.to_le_bytes());
            body.extend_from_slice(&((12 + payload.len()) as u16).to_le_bytes());
            body.extend_from_slice(&code.to_le_bytes());
            body.extend_from_slice(&st.ts.to_le_bytes());
            body.extend_from_slice(payload);
        }
        // `[i32 0x40][i32 token][i32 total]` then the records.
        let mut msg = Vec::new();
        for v in [DCI_DATA, 1, body.len() as i32] {
            msg.extend_from_slice(&v.to_le_bytes());
        }
        msg.extend_from_slice(&body);
        (&*conn).write_all(&msg).expect("push_logs: write to client");
    }

    /// Every subscription write received so far, across all connections, in order.
    pub fn subscriptions(&self) -> Vec<(bool, Vec<u16>)> {
        self.quiesced().subscriptions.clone()
    }

    /// Every register body received so far (20 bytes each), across all connections.
    pub fn registrations(&self) -> Vec<[u8; 20]> {
        self.quiesced().registrations.clone()
    }

    /// Client ids of every deregister message received so far, across all connections.
    pub fn deregistrations(&self) -> Vec<i32> {
        self.quiesced().deregistrations.clone()
    }

    /// Stop reading every connection, current and future, without closing any: messages
    /// already sent are processed first, later ones stay queued and the sender eventually
    /// blocks. Handshakes on new connections get no reply.
    pub fn stop_reading(&self) {
        let _st = self.quiesced();
        self.shared.deaf.store(true, Ordering::Relaxed);
    }

    /// From now on, answer no support query: the client's handshake fails after it has
    /// registered and been given a client id.
    pub fn ignore_support(&self) {
        self.shared.mute_support.store(true, Ordering::Relaxed);
    }

    /// From now on, answer no handshake message and instead send the client an untagged
    /// message every `every`: each read returns, so only an overall deadline ends the wait.
    pub fn chatter(&self, every: Duration) {
        self.shared.chatter_ms.store(every.as_millis().max(1) as u64, Ordering::Relaxed);
    }

    /// Close the current client connection (after processing what it sent). The router keeps
    /// listening and accepts the next connection.
    pub fn drop_client(&self) {
        let conn = {
            let mut st = self.quiesced();
            st.client.take()
        };
        if let Some(c) = conn {
            unsafe { shutdown(c.as_raw_fd(), SHUT_RDWR) };
        }
    }
}

impl Drop for FakeRouter {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.accept_thread.take() {
            let _ = h.join();
        }
        if let Some(h) = self.chatter_thread.take() {
            let _ = h.join();
        }
        // The accept thread has shut down and joined every connection; closing the listener
        // releases the abstract name.
        unsafe { close(self.listen_fd) };
    }
}
