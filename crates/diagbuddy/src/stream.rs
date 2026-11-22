// SPDX-License-Identifier: Apache-2.0
//! Local stream server: carries a raw-HDLC DIAG stream to up to `max_clients` clients at once.
//!
//! Outbound frames arrive already wire-ready (HDLC-encoded, ending in `0x7E`). [`Client::send`]
//! never blocks: it pushes onto a bounded queue and, when the queue is full, drops the oldest
//! whole frame. A writer thread drains the queue, so a stalled peer costs dropped frames and
//! nothing else. Inbound bytes are split on `0x7E`, un-escaped and CRC-checked here.
//!
//! Each client has its own queue and writer thread, so one stalled client drops only its own
//! frames. A background accept thread admits up to `max_clients` attached clients; a connection
//! that arrives while that many are attached is closed at once and counted in
//! [`Listener::refused`], so the caller can log it. A client counts as detached when its reader
//! has seen EOF or an error, its writer has failed or timed out, or it was dropped.

use std::collections::VecDeque;
use std::fs;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use dcicore::frame::hdlc_decode;

use crate::diagcmd::MAX_REQUEST;

/// Frames held for a slow client before the oldest is dropped.
pub const QUEUE_FRAMES: usize = 4096;
/// Attached clients a listener admits when the profile does not say.
pub const DEFAULT_MAX_CLIENTS: usize = 8;
/// Most clients a profile may allow. Each costs two threads and up to [`QUEUE_FRAMES`] frames.
pub const MAX_CLIENTS_LIMIT: usize = 64;

/// A write that makes no progress for this long detaches the client.
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
/// How often an idle accept thread looks for a connection and at the stop flag: five
/// wakeups a second. A connection waits at most this long to be taken.
const ACCEPT_POLL: Duration = Duration::from_millis(200);
/// Pause after an accept error that is not "nothing waiting" (aborted connection, fd pressure).
const ACCEPT_ERROR_PAUSE: Duration = Duration::from_millis(100);
const FLAG: u8 = 0x7E;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counters {
    /// Frames fully written to the socket.
    pub sent: u64,
    /// Frames discarded: queue full (oldest dropped) or client already detached.
    pub dropped: u64,
    /// Requests discarded for a bad CRC, a bad escape, or exceeding `MAX_REQUEST`.
    pub bad_requests: u64,
}

// ---- socket abstraction -------------------------------------------------------------

enum Conn {
    Tcp(TcpStream),
    Unix(UnixStream),
}

impl Conn {
    fn try_clone(&self) -> io::Result<Conn> {
        Ok(match self {
            Conn::Tcp(s) => Conn::Tcp(s.try_clone()?),
            Conn::Unix(s) => Conn::Unix(s.try_clone()?),
        })
    }

    fn shutdown(&self) {
        let _ = match self {
            Conn::Tcp(s) => s.shutdown(std::net::Shutdown::Both),
            Conn::Unix(s) => s.shutdown(std::net::Shutdown::Both),
        };
    }

    fn set_write_timeout(&self, t: Option<Duration>) -> io::Result<()> {
        match self {
            Conn::Tcp(s) => s.set_write_timeout(t),
            Conn::Unix(s) => s.set_write_timeout(t),
        }
    }
}

impl Read for Conn {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Conn::Tcp(s) => s.read(buf),
            Conn::Unix(s) => s.read(buf),
        }
    }
}

impl Write for Conn {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Conn::Tcp(s) => s.write(buf),
            Conn::Unix(s) => s.write(buf),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

enum Bound {
    Tcp(TcpListener),
    Unix(UnixListener),
}

impl Bound {
    /// Non-blocking accept; `WouldBlock` when nothing is waiting.
    fn accept(&self) -> io::Result<Conn> {
        let conn = match self {
            Bound::Tcp(l) => Conn::Tcp(l.accept()?.0),
            Bound::Unix(l) => Conn::Unix(l.accept()?.0),
        };
        // Some platforms hand back sockets that inherit the listener's non-blocking mode.
        match &conn {
            Conn::Tcp(s) => s.set_nonblocking(false)?,
            Conn::Unix(s) => s.set_nonblocking(false)?,
        }
        Ok(conn)
    }
}

// ---- listener -----------------------------------------------------------------------

pub struct Listener {
    addr: String,
    unix_path: Option<PathBuf>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    clients: Mutex<Receiver<Client>>,
    refused: Arc<AtomicU64>,
    max_clients: usize,
}

fn invalid(msg: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, msg)
}

impl Listener {
    /// `tcp:<ip>:<port>` (loopback IP literals only) or `unix:<path>` (mode 0600), admitting
    /// at most `max_clients` (1..=[`MAX_CLIENTS_LIMIT`]) attached clients at once.
    pub fn bind(listen: &str, max_clients: usize) -> io::Result<Listener> {
        if !(1..=MAX_CLIENTS_LIMIT).contains(&max_clients) {
            return Err(invalid(format!(
                "max_clients {max_clients} must be between 1 and {MAX_CLIENTS_LIMIT}"
            )));
        }
        let (bound, addr, unix_path) = if let Some(rest) = listen.strip_prefix("tcp:") {
            let sa: SocketAddr = rest.parse().map_err(|_| {
                invalid(format!(
                    "tcp listen address {rest:?} must be an IP literal and port \
                     (hostnames, including localhost, are refused)"
                ))
            })?;
            if !sa.ip().is_loopback() {
                return Err(invalid(format!("tcp listen address {sa} is not loopback")));
            }
            let l = TcpListener::bind(sa)?;
            let addr = format!("tcp:{}", l.local_addr()?);
            (Bound::Tcp(l), addr, None)
        } else if let Some(path) = listen.strip_prefix("unix:") {
            let path = PathBuf::from(path);
            let l = bind_unix(&path)?;
            (Bound::Unix(l), format!("unix:{}", path.display()), Some(path))
        } else {
            return Err(invalid(format!(
                "listen address {listen:?} must start with tcp: or unix:"
            )));
        };
        match &bound {
            Bound::Tcp(l) => l.set_nonblocking(true)?,
            Bound::Unix(l) => l.set_nonblocking(true)?,
        }

        let stop = Arc::new(AtomicBool::new(false));
        let refused = Arc::new(AtomicU64::new(0));
        let (tx, rx) = mpsc::channel();
        let thread = {
            let stop = stop.clone();
            let refused = refused.clone();
            thread::Builder::new().name("stream-accept".into()).spawn(move || {
                let mut live: Vec<Arc<ClientShared>> = Vec::new();
                while !stop.load(Ordering::Relaxed) {
                    match bound.accept() {
                        Ok(conn) => {
                            live.retain(|c| !c.is_closed());
                            if live.len() >= max_clients {
                                conn.shutdown(); // full: refused, and counted for the caller
                                refused.fetch_add(1, Ordering::Relaxed);
                                continue;
                            }
                            match Client::new(conn) {
                                Ok(client) => {
                                    live.push(client.shared.clone());
                                    if tx.send(client).is_err() {
                                        return; // listener gone
                                    }
                                }
                                Err(_) => continue, // peer vanished during setup
                            }
                        }
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => thread::sleep(ACCEPT_POLL),
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                        // Transient (aborted connection, fd pressure): back off, keep serving.
                        Err(_) => thread::sleep(ACCEPT_ERROR_PAUSE),
                    }
                }
            })?
        };

        Ok(Listener {
            addr,
            unix_path,
            stop,
            thread: Some(thread),
            clients: Mutex::new(rx),
            refused,
            max_clients,
        })
    }

    /// Connections closed on arrival because `max_clients` were already attached (lifetime).
    pub fn refused(&self) -> u64 {
        self.refused.load(Ordering::Relaxed)
    }

    pub fn max_clients(&self) -> usize {
        self.max_clients
    }

    /// Block until a client attaches.
    pub fn accept(&self) -> io::Result<Client> {
        lock(&self.clients)
            .recv()
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "accept thread stopped"))
    }

    /// Like [`Listener::accept`], but give up after `timeout`: `Ok(None)` when no client
    /// attached in time. Lets a caller poll a stop flag between waits.
    pub fn accept_timeout(&self, timeout: Duration) -> io::Result<Option<Client>> {
        match lock(&self.clients).recv_timeout(timeout) {
            Ok(c) => Ok(Some(c)),
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(RecvTimeoutError::Disconnected) => {
                Err(io::Error::new(io::ErrorKind::BrokenPipe, "accept thread stopped"))
            }
        }
    }

    /// The bound address in the same form `bind` takes (`tcp:ip:port`, `unix:path`).
    pub fn local_addr(&self) -> String {
        self.addr.clone()
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        if let Some(p) = &self.unix_path {
            let _ = fs::remove_file(p);
        }
    }
}

/// Bind a unix socket: create the parent, replace a stale *socket* file (never anything
/// else), and restrict the socket to its owner.
fn bind_unix(path: &Path) -> io::Result<UnixListener> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    match fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_socket() => fs::remove_file(path)?,
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("{} exists and is not a socket", path.display()),
            ))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let l = UnixListener::bind(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(l)
}

// ---- client -------------------------------------------------------------------------

struct ClientShared {
    queue: Mutex<VecDeque<Vec<u8>>>,
    wake: Condvar,
    closed: AtomicBool,
    sock: Conn,
    sent: AtomicU64,
    dropped: AtomicU64,
    bad_requests: AtomicU64,
}

impl ClientShared {
    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    /// Detach: idempotent. Closing the socket unblocks a reader or writer stuck in a syscall.
    fn close(&self) {
        if !self.closed.swap(true, Ordering::AcqRel) {
            self.sock.shutdown();
        }
        // Take the queue lock so a writer between its check and its wait cannot miss this.
        drop(lock(&self.queue));
        self.wake.notify_all();
    }
}

pub struct Client {
    shared: Arc<ClientShared>,
    writer: Option<JoinHandle<()>>,
}

impl Client {
    fn new(conn: Conn) -> io::Result<Client> {
        if let Conn::Tcp(s) = &conn {
            s.set_nodelay(true)?;
        }
        conn.set_write_timeout(Some(WRITE_TIMEOUT))?;
        let writer_sock = conn.try_clone()?;
        let shared = Arc::new(ClientShared {
            queue: Mutex::new(VecDeque::with_capacity(QUEUE_FRAMES)),
            wake: Condvar::new(),
            closed: AtomicBool::new(false),
            sock: conn,
            sent: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            bad_requests: AtomicU64::new(0),
        });
        let writer = {
            let shared = shared.clone();
            thread::Builder::new()
                .name("stream-writer".into())
                .spawn(move || write_loop(&shared, writer_sock))?
        };
        Ok(Client { shared, writer: Some(writer) })
    }

    /// Queue one wire-ready frame. Never blocks on the peer; when the queue is full the
    /// oldest queued frame is dropped (a frame is dropped whole or sent whole).
    pub fn send(&self, frame: Vec<u8>) {
        if self.shared.is_closed() {
            self.shared.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let evicted = {
            let mut q = lock(&self.shared.queue);
            let evicted = if q.len() >= QUEUE_FRAMES { q.pop_front() } else { None };
            q.push_back(frame);
            evicted
        };
        if evicted.is_some() {
            self.shared.dropped.fetch_add(1, Ordering::Relaxed);
        }
        self.shared.wake.notify_one();
    }

    /// Decoded request packets from the client: HDLC framing and CRC removed. Ends when the
    /// client disconnects. Call it once; each call starts with its own empty buffer.
    pub fn requests(&self) -> impl Iterator<Item = Vec<u8>> {
        Requests {
            shared: self.shared.clone(),
            sock: self.shared.sock.try_clone().ok(),
            splitter: Splitter::default(),
            ready: VecDeque::new(),
        }
    }

    pub fn counters(&self) -> Counters {
        Counters {
            sent: self.shared.sent.load(Ordering::Relaxed),
            dropped: self.shared.dropped.load(Ordering::Relaxed),
            bad_requests: self.shared.bad_requests.load(Ordering::Relaxed),
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.shared.close();
        if let Some(w) = self.writer.take() {
            let _ = w.join();
        }
    }
}

fn write_loop(shared: &ClientShared, mut sock: Conn) {
    loop {
        let frame = {
            let mut q = lock(&shared.queue);
            loop {
                if shared.is_closed() {
                    return;
                }
                if let Some(f) = q.pop_front() {
                    break f;
                }
                q = shared.wake.wait(q).unwrap_or_else(|e| e.into_inner());
            }
        };
        // Written outside the lock; a failure, timeout or EPIPE is a normal disconnect.
        match sock.write_all(&frame) {
            Ok(()) => {
                shared.sent.fetch_add(1, Ordering::Relaxed);
            }
            Err(_) => {
                shared.close();
                return;
            }
        }
    }
}

// ---- inbound requests ---------------------------------------------------------------

/// Splits a byte stream on `0x7E` and decodes each frame. Memory is bounded: no more than
/// `MAX_REQUEST` raw bytes are held between flags.
#[derive(Default)]
struct Splitter {
    buf: Vec<u8>,
    /// Skipping the rest of an oversized frame, up to the next flag.
    discarding: bool,
}

impl Splitter {
    /// Feed bytes; push each good packet onto `out` and return how many frames were bad.
    fn feed(&mut self, data: &[u8], out: &mut VecDeque<Vec<u8>>) -> u64 {
        let mut bad = 0;
        for &b in data {
            if self.discarding {
                self.discarding = b != FLAG;
                continue;
            }
            if b == FLAG {
                if self.buf.is_empty() {
                    continue; // empty frame between consecutive flags
                }
                self.buf.push(FLAG);
                match hdlc_decode(&self.buf) {
                    Some(pkt) => out.push_back(pkt),
                    None => bad += 1,
                }
                self.buf.clear();
            } else if self.buf.len() >= MAX_REQUEST {
                bad += 1;
                self.buf.clear();
                self.discarding = true;
            } else {
                self.buf.push(b);
            }
        }
        bad
    }
}

struct Requests {
    shared: Arc<ClientShared>,
    sock: Option<Conn>,
    splitter: Splitter,
    ready: VecDeque<Vec<u8>>,
}

impl Iterator for Requests {
    type Item = Vec<u8>;

    fn next(&mut self) -> Option<Vec<u8>> {
        let mut chunk = [0u8; 2048];
        loop {
            if let Some(p) = self.ready.pop_front() {
                return Some(p);
            }
            let sock = self.sock.as_mut()?;
            match sock.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    let bad = self.splitter.feed(&chunk[..n], &mut self.ready);
                    if bad > 0 {
                        self.shared.bad_requests.fetch_add(bad, Ordering::Relaxed);
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
        // EOF or error: the client is gone; free the slot and stop the writer.
        self.sock = None;
        self.shared.close();
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagcmd::MAX_REQUEST;
    use dcicore::frame::hdlc_encode;
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixStream;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    const WAIT: Duration = Duration::from_secs(5);

    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> TempDir {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos();
            let p = std::env::temp_dir().join(format!("dbs-{tag}-{}-{nanos}", std::process::id()));
            std::fs::create_dir_all(&p).unwrap();
            TempDir(p)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn tcp_listener() -> Listener {
        Listener::bind("tcp:127.0.0.1:0", DEFAULT_MAX_CLIENTS).unwrap()
    }

    fn tcp_connect(l: &Listener) -> TcpStream {
        let addr = l.local_addr();
        let s = TcpStream::connect(addr.strip_prefix("tcp:").unwrap()).unwrap();
        s.set_read_timeout(Some(WAIT)).unwrap();
        s
    }

    fn encoded(pkt: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        hdlc_encode(pkt, &mut v);
        v
    }

    /// Read exactly `n` bytes.
    fn read_n(s: &mut impl Read, n: usize) -> Vec<u8> {
        let mut buf = vec![0u8; n];
        s.read_exact(&mut buf).unwrap();
        buf
    }

    #[test]
    fn frames_reach_the_client_whole_and_in_order() {
        let l = tcp_listener();
        let mut peer = tcp_connect(&l);
        let c = l.accept().unwrap();
        let mut want = Vec::new();
        for i in 0..50u8 {
            let f = encoded(&[0x10, 0x00, i, 0x7E, 0x7D, i]);
            want.extend_from_slice(&f);
            c.send(f);
        }
        assert_eq!(read_n(&mut peer, want.len()), want);
        // `sent` is bumped after write_all returns, so the peer can finish reading a
        // moment before the last increment: wait for it to settle, then require all 50.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while c.counters().sent < 50 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let ctr = c.counters();
        assert_eq!(ctr.dropped, 0);
        assert_eq!(ctr.sent, 50);
    }

    #[test]
    fn frames_reach_a_unix_client_and_the_socket_is_private() {
        let dir = TempDir::new("unix");
        let path = dir.0.join("sub").join("diag.sock");
        let l = Listener::bind(&format!("unix:{}", path.display()), 1).unwrap();
        assert_eq!(l.local_addr(), format!("unix:{}", path.display()));
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let mut peer = UnixStream::connect(&path).unwrap();
        peer.set_read_timeout(Some(WAIT)).unwrap();
        let c = l.accept().unwrap();
        let f = encoded(&[0x10, 1, 2, 3]);
        c.send(f.clone());
        assert_eq!(read_n(&mut peer, f.len()), f);
        drop(c);
        drop(l);
        assert!(!path.exists(), "dropping the listener removes the socket file");
    }

    #[test]
    fn a_stale_socket_file_is_replaced_but_other_files_are_not() {
        let dir = TempDir::new("stale");
        let path = dir.0.join("diag.sock");
        drop(std::os::unix::net::UnixListener::bind(&path).unwrap()); // leaves the file behind
        assert!(path.exists());
        let l = Listener::bind(&format!("unix:{}", path.display()), 1).unwrap();
        drop(l);

        let plain = dir.0.join("not-a-socket");
        std::fs::write(&plain, b"keep me").unwrap();
        assert!(Listener::bind(&format!("unix:{}", plain.display()), 1).is_err());
        assert_eq!(std::fs::read(&plain).unwrap(), b"keep me");
    }

    #[test]
    fn a_client_that_never_reads_costs_only_dropped_frames() {
        let l = tcp_listener();
        let _peer = tcp_connect(&l); // connects, never reads
        let c = l.accept().unwrap();
        const N: u32 = 100_000;
        let mut total = Duration::ZERO;
        for _ in 0..N {
            let mut frame = vec![0x55u8; 511];
            frame.push(0x7E);
            let t = Instant::now();
            c.send(frame);
            total += t.elapsed();
        }
        let avg = total / N;
        assert!(avg < Duration::from_millis(1), "average send took {avg:?}");
        let ctr = c.counters();
        assert!(ctr.dropped > 0, "{ctr:?}");
    }

    #[test]
    fn frames_that_arrive_after_drops_are_whole_and_the_newest_survives() {
        let l = tcp_listener();
        let mut peer = tcp_connect(&l);
        let c = l.accept().unwrap();
        const N: usize = 20_000;
        // Frame i is 512 bytes: 511 copies of a byte derived from i, then the flag.
        let fill = |i: usize| (i % 100) as u8 + 1;
        for i in 0..N {
            let mut frame = vec![fill(i); 511];
            frame.push(0x7E);
            c.send(frame);
        }
        assert!(c.counters().dropped > 0);
        // Now read everything until the stream goes quiet.
        peer.set_read_timeout(Some(Duration::from_millis(500))).unwrap();
        let mut got = Vec::new();
        let mut buf = [0u8; 65536];
        loop {
            match peer.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => got.extend_from_slice(&buf[..n]),
                Err(_) => break,
            }
        }
        assert!(!got.is_empty());
        assert_eq!(got.len() % 512, 0, "a partial frame reached the client");
        let mut last = 0;
        for chunk in got.chunks(512) {
            assert_eq!(chunk[511], 0x7E);
            assert!(chunk[..511].iter().all(|&b| b == chunk[0]), "torn frame");
            last = chunk[0];
        }
        assert_eq!(last, fill(N - 1), "the newest frame must survive drop-oldest");
    }

    #[test]
    fn frames_sent_before_a_stall_arrive_intact_and_in_order() {
        let l = tcp_listener();
        let mut peer = tcp_connect(&l);
        let c = l.accept().unwrap();
        const K: usize = 300;
        let frame = |i: usize| {
            let mut f = vec![(i % 200) as u8 + 1; 511];
            f.push(0x7E);
            f
        };
        for i in 0..K {
            c.send(frame(i));
        }
        let got = read_n(&mut peer, K * 512);
        for (i, chunk) in got.chunks(512).enumerate() {
            assert_eq!(chunk, &frame(i)[..], "frame {i}");
        }
        assert_eq!(c.counters().dropped, 0);
        // Now the peer stalls; flooding costs dropped frames.
        for i in 0..20_000 {
            c.send(frame(i));
        }
        assert!(c.counters().dropped > 0);
    }

    #[test]
    fn requests_are_unescaped_and_crc_checked() {
        let l = tcp_listener();
        let mut peer = tcp_connect(&l);
        let c = l.accept().unwrap();
        let good = [0x73u8, 0x7E, 0x7D, 0x00, 0x01];
        peer.write_all(&encoded(&good)).unwrap();
        // Flip a payload byte: the FCS no longer matches.
        let mut bad = encoded(&[0x73, 0x01, 0x02, 0x03]);
        bad[1] ^= 0x01;
        peer.write_all(&bad).unwrap();
        // Back-to-back flags are empty frames, skipped without counting.
        peer.write_all(&[0x7E, 0x7E, 0x7E]).unwrap();
        let good2 = [0x73u8, 0x09];
        // Deliver the last frame split across two writes.
        let e = encoded(&good2);
        peer.write_all(&e[..2]).unwrap();
        peer.flush().unwrap();
        std::thread::sleep(Duration::from_millis(30));
        peer.write_all(&e[2..]).unwrap();
        drop(peer);
        let got: Vec<Vec<u8>> = c.requests().collect();
        assert_eq!(got, vec![good.to_vec(), good2.to_vec()]);
        assert_eq!(c.counters().bad_requests, 1);
    }

    #[test]
    fn an_oversized_request_is_dropped() {
        let l = tcp_listener();
        let mut peer = tcp_connect(&l);
        let c = l.accept().unwrap();
        let mut big = vec![0x41u8; MAX_REQUEST + 100];
        big.push(0x7E);
        peer.write_all(&big).unwrap();
        let ok = [0x73u8, 0x00, 0x00];
        peer.write_all(&encoded(&ok)).unwrap();
        drop(peer);
        let got: Vec<Vec<u8>> = c.requests().collect();
        assert_eq!(got, vec![ok.to_vec()]);
        assert_eq!(c.counters().bad_requests, 1, "one oversize frame is one bad request");
    }

    #[test]
    fn a_stream_with_no_flag_is_never_buffered_without_bound() {
        let l = tcp_listener();
        let mut peer = tcp_connect(&l);
        let c = l.accept().unwrap();
        let junk = vec![0x41u8; 1 << 20];
        peer.write_all(&junk).unwrap();
        let ok = [0x73u8, 0x07];
        peer.write_all(&[0x7E]).unwrap();
        peer.write_all(&encoded(&ok)).unwrap();
        drop(peer);
        let got: Vec<Vec<u8>> = c.requests().collect();
        assert_eq!(got, vec![ok.to_vec()]);
        assert_eq!(c.counters().bad_requests, 1);
    }

    fn closed_at_once(s: &mut TcpStream) -> bool {
        let mut b = [0u8; 1];
        let r = s.read(&mut b);
        matches!(&r, Ok(0)) || r.as_ref().is_err_and(|e| e.kind() == io::ErrorKind::ConnectionReset)
    }

    #[test]
    fn clients_up_to_the_limit_are_served_at_once_and_one_more_is_refused_and_counted() {
        let l = Listener::bind("tcp:127.0.0.1:0", 3).unwrap();
        let mut peers = Vec::new();
        let mut clients = Vec::new();
        for _ in 0..3 {
            peers.push(tcp_connect(&l));
            clients.push(l.accept().unwrap());
        }
        assert_eq!(l.refused(), 0);
        // Every attached client gets its own frames, whole, at the same time.
        for (i, c) in clients.iter().enumerate() {
            c.send(encoded(&[0x10, i as u8]));
        }
        for (i, p) in peers.iter_mut().enumerate() {
            let f = encoded(&[0x10, i as u8]);
            assert_eq!(read_n(p, f.len()), f, "client {i}");
        }
        // A fourth is closed at once and counted; the three are untouched.
        let mut fourth = tcp_connect(&l);
        assert!(closed_at_once(&mut fourth), "the client over the limit must be closed");
        assert_eq!(l.refused(), 1);
        clients[0].send(encoded(&[1, 2, 3]));
        let f = encoded(&[1, 2, 3]);
        assert_eq!(read_n(&mut peers[0], f.len()), f);
        // One leaves: its slot is free for the next.
        drop(clients.remove(1));
        let mut next = tcp_connect(&l);
        let c = l.accept().unwrap();
        let f = encoded(&[9, 9]);
        c.send(f.clone());
        assert_eq!(read_n(&mut next, f.len()), f);
        assert_eq!(l.refused(), 1);
    }

    #[test]
    fn a_limit_of_one_closes_a_second_connection_while_one_is_attached() {
        let l = Listener::bind("tcp:127.0.0.1:0", 1).unwrap();
        let _first = tcp_connect(&l);
        let c = l.accept().unwrap();
        let mut second = tcp_connect(&l);
        assert!(closed_at_once(&mut second));
        assert_eq!(l.refused(), 1);
        drop(c);
        let mut third = tcp_connect(&l);
        let c3 = l.accept().unwrap();
        let f = encoded(&[9, 9]);
        c3.send(f.clone());
        assert_eq!(read_n(&mut third, f.len()), f);
    }

    #[test]
    fn a_stalled_client_does_not_hold_up_another() {
        let l = tcp_listener();
        let _stalled = tcp_connect(&l); // never reads
        let a = l.accept().unwrap();
        let mut live = tcp_connect(&l);
        let b = l.accept().unwrap();
        let frame = |i: usize| {
            let mut f = vec![(i % 200) as u8 + 1; 511];
            f.push(0x7E);
            f
        };
        // Rounds smaller than the queue, so the reading client keeps up; the stalled one
        // accumulates far more than its queue and socket buffer hold.
        const ROUND: usize = 1000;
        const ROUNDS: usize = 20;
        let t = Instant::now();
        for r in 0..ROUNDS {
            for i in r * ROUND..(r + 1) * ROUND {
                a.send(frame(i));
                b.send(frame(i));
            }
            let got = read_n(&mut live, ROUND * 512);
            for (k, chunk) in got.chunks(512).enumerate() {
                assert_eq!(chunk, &frame(r * ROUND + k)[..], "round {r} frame {k}");
            }
        }
        assert!(t.elapsed() < Duration::from_secs(10), "{:?}", t.elapsed());
        assert_eq!(b.counters().dropped, 0, "the reading client lost nothing");
        assert!(a.counters().dropped > 0, "the stalled client's drops are its own");
    }

    #[test]
    fn a_max_clients_outside_the_range_is_refused() {
        assert!(Listener::bind("tcp:127.0.0.1:0", 0).is_err());
        assert!(Listener::bind("tcp:127.0.0.1:0", MAX_CLIENTS_LIMIT + 1).is_err());
        assert!(Listener::bind("tcp:127.0.0.1:0", MAX_CLIENTS_LIMIT).is_ok());
    }

    #[test]
    fn a_client_that_disconnects_ends_requests_and_frees_the_slot() {
        let l = Listener::bind("tcp:127.0.0.1:0", 1).unwrap();
        let peer = tcp_connect(&l);
        let c = l.accept().unwrap();
        drop(peer);
        assert_eq!(c.requests().count(), 0);
        // `c` is still held, but its reader saw EOF, so the next connection is accepted.
        let mut next = tcp_connect(&l);
        let c2 = l.accept().unwrap();
        let f = encoded(&[4, 5, 6]);
        c2.send(f.clone());
        assert_eq!(read_n(&mut next, f.len()), f);
        drop(c);
    }

    #[test]
    fn dropping_the_client_closes_the_socket() {
        let l = tcp_listener();
        let mut peer = tcp_connect(&l);
        let c = l.accept().unwrap();
        drop(c);
        let mut b = [0u8; 1];
        let r = peer.read(&mut b);
        assert!(
            matches!(&r, Ok(0)) || r.as_ref().is_err_and(|e| e.kind() == io::ErrorKind::ConnectionReset),
            "{r:?}"
        );
    }

    #[test]
    fn accept_timeout_returns_none_when_idle_and_the_client_when_one_connects() {
        let l = tcp_listener();
        let t = Instant::now();
        assert!(l.accept_timeout(Duration::from_millis(30)).unwrap().is_none());
        assert!(t.elapsed() >= Duration::from_millis(30));
        let mut peer = tcp_connect(&l);
        let c = wait_accept(&l);
        let f = encoded(&[7, 7]);
        c.send(f.clone());
        assert_eq!(read_n(&mut peer, f.len()), f);
    }

    fn wait_accept(l: &Listener) -> Client {
        let deadline = Instant::now() + WAIT;
        loop {
            if let Some(c) = l.accept_timeout(Duration::from_millis(10)).unwrap() {
                return c;
            }
            assert!(Instant::now() < deadline, "no client accepted");
        }
    }

    #[test]
    fn dropping_the_listener_stops_accepting() {
        let l = tcp_listener();
        let addr = l.local_addr();
        let addr = addr.strip_prefix("tcp:").unwrap().to_string();
        drop(l);
        assert!(TcpStream::connect(&addr).is_err());
    }

    #[test]
    fn a_non_loopback_tcp_address_is_refused() {
        let bind = |a: &str| Listener::bind(a, DEFAULT_MAX_CLIENTS);
        assert!(bind("tcp:0.0.0.0:43555").is_err());
        assert!(bind("tcp:192.0.2.1:43555").is_err());
        assert!(bind("tcp:[::]:43555").is_err());
        let e = bind("tcp:localhost:43555").err().unwrap();
        assert!(e.to_string().contains("IP literal"), "{e}");
        assert!(bind("tcp:127.0.0.1:0").is_ok());
        assert!(bind("udp:127.0.0.1:1").is_err());
    }
}
