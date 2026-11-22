// SPDX-License-Identifier: Apache-2.0
//! The daemon: one DCI session shared by several stream clients, kept alive across router loss.
//!
//! Each stream client has its own [`Responder`] (its own LOG_CONFIG masks and replies) and its
//! own set of log codes. The DCI subscription is the union of every attached client's codes
//! and the profile's `default_codes`, capped at [`MAX_SUBSCRIBED_CODES`]. A record goes only to
//! the clients whose own codes include it: a client never receives a code because another
//! client (or the defaults) asked for it.
//!
//! Threads:
//! * **supervisor** (the caller of [`run`]): registers with DCI and retries with backoff,
//!   accepts clients, rewrites the status file, and on stop tears everything down.
//! * **DCI reader**, one per registration: blocking reads on its own handle; each log record is
//!   framed and queued on every client that wants its code ([`Client::send`] never blocks). It
//!   holds no lock while reading. On EOF or a read error it marks the session lost.
//! * **client requests**, one per client: answers each request through that client's
//!   [`Responder`] and turns a changed mask into DCI subscribe / unsubscribe messages for the
//!   new union. On disconnect it drops the codes no other client wants.
//!
//! Locks: `core` (DCI session, subscription, per-client responders) is taken before `clients`
//! (the attached clients and their delivery filters), never the other way round. The reader
//! takes only `clients`, so `clients` is never held across a DCI write: a router blocked on
//! sending us data must still be drained.
//!
//! diagbuddy writes nothing to the stack but its own registration and log subscription, and
//! only observes stock DIAG consumers (by reading `/proc`).
//!
//! Every line the daemon logs goes through [`Events::log`]: to stderr, and into a ring of the
//! last [`EVENTS_KEPT`] lines that the status file carries, so they outlive the syslog ring.
//! A line holds only the daemon's own state, log codes and counters, never modem data.

use crate::diagcmd::Responder;
use crate::profile::{Profile, SessionMode, Transport as ProfileTransport};
use crate::stream::{Client, Counters, Listener};
use dcicore::dci::{log_mask_msg, parse_data};
use dcicore::frame::{frame_log, hdlc_encode, Outer};
use dcicore::sys::ROUTER_NAME;
use dcicore::transport::Transport;
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const DEFAULT_STATUS_FILE: &str = "/var/run/rayhunter/diagbuddy.status.json";
/// First retry delay after a lost or failed registration; doubles per failure.
pub const RETRY_BASE: Duration = Duration::from_secs(1);
pub const RETRY_CAP: Duration = Duration::from_secs(30);
/// A session that lived at least this long resets the retry delay to the base when lost; a
/// shorter one counts as a failure, so a router that drops every session at once is retried
/// ever more slowly instead of once a second for ever.
pub const MIN_SESSION: Duration = Duration::from_secs(60);
pub const STATUS_INTERVAL: Duration = Duration::from_secs(5);
/// A change marks the status dirty, but it is rewritten no sooner than this after the last
/// write, so a request flood costs at most one rewrite a second, not one per tick.
pub const STATUS_MIN_GAP: Duration = Duration::from_secs(1);
/// Most log codes the DCI subscription may hold, the client's and the defaults together. One
/// whole equipment id (4095 codes, about 8 KB in one message) is the largest subscription
/// measured on a unit; raise this only once a larger one has been measured.
pub const MAX_SUBSCRIBED_CODES: usize = 4096;
/// Log lines kept for the status file's `events`.
pub const EVENTS_KEPT: usize = 20;
/// Log lines that may wait for a stalled stderr. Past this, stderr loses lines (the ring
/// still has the newest [`EVENTS_KEPT`]); the caller never waits.
pub const STDERR_QUEUE: usize = 256;
/// How long the supervisor waits for a client before looking at everything else again.
const TICK: Duration = Duration::from_millis(50);
/// One DCI read. A seqpacket message longer than the buffer would be truncated.
const READ_BUF: usize = 256 * 1024;

/// A stop request: the signal handler's path, and what tests call directly.
#[derive(Clone, Default, Debug)]
pub struct Stop(Arc<AtomicBool>);

impl Stop {
    pub fn request(&self) {
        self.0.store(true, Ordering::Release);
    }

    pub fn requested(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

/// A re-registration request (the SIGUSR2 path, and what tests call directly): the supervisor
/// deregisters the live DCI session and registers again from the same process, at once and
/// without counting a failure. A debug aid: it is how to find out whether the router accepts a
/// second registration from a pid it has already seen, which no restart can show.
#[derive(Clone, Default, Debug)]
pub struct Reregister(Arc<AtomicBool>);

impl Reregister {
    pub fn request(&self) {
        self.0.store(true, Ordering::Release);
    }

    /// Consume a pending request.
    fn take(&self) -> bool {
        self.0.swap(false, Ordering::AcqRel)
    }
}

pub struct Opts {
    pub status_file: PathBuf,
    /// Abstract socket name (without the leading NUL) to use instead of the profile's
    /// transport. Tests point it at a `FakeRouter`.
    pub router_addr: Option<Vec<u8>>,
    /// Resolved host and modem profile names, reported in the status file.
    pub host: String,
    pub modem: String,
    pub stop: Stop,
    pub reregister: Reregister,
    pub retry_base: Duration,
    /// See [`MIN_SESSION`].
    pub min_session: Duration,
    pub status_interval: Duration,
    /// See [`STATUS_MIN_GAP`].
    pub status_min_gap: Duration,
    /// Where to look for stock consumers' `<pid>/comm`.
    pub proc_root: PathBuf,
}

impl Opts {
    pub fn new(status_file: PathBuf) -> Opts {
        Opts {
            status_file,
            router_addr: None,
            host: "auto".into(),
            modem: "auto".into(),
            stop: Stop::default(),
            reregister: Reregister::default(),
            retry_base: RETRY_BASE,
            min_session: MIN_SESSION,
            status_interval: STATUS_INTERVAL,
            status_min_gap: STATUS_MIN_GAP,
            proc_root: PathBuf::from("/proc"),
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The daemon's log: stderr plus the last [`EVENTS_KEPT`] lines, each with its time.
///
/// stderr is written by a thread of its own. Callers log with the `core` lock held, and
/// procd's stderr pipe can stall; a write on the caller's thread would then stall the
/// supervisor and every request with it. The queue is bounded and never waited on.
struct Events {
    ring: Mutex<VecDeque<(String, String)>>,
    out: Mutex<Option<SyncSender<String>>>,
    writer: Mutex<Option<thread::JoinHandle<()>>>,
}

impl Events {
    fn new() -> Events {
        Events::with_sink(Box::new(io::stderr()))
    }

    fn with_sink(mut sink: Box<dyn Write + Send>) -> Events {
        let (tx, rx) = sync_channel::<String>(STDERR_QUEUE);
        let writer = thread::Builder::new()
            .name("diagbuddy-log".into())
            .spawn(move || {
                for line in rx {
                    let _ = writeln!(sink, "diagbuddy: {line}");
                }
                let _ = sink.flush();
            })
            .ok();
        // No writer thread: `out` stays empty and log() writes stderr itself.
        let out = writer.as_ref().map(|_| tx);
        Events { ring: Mutex::new(VecDeque::new()), out: Mutex::new(out), writer: Mutex::new(writer) }
    }

    /// The one way the daemon logs. Both locks here are leaves: nothing else is taken under
    /// them, and neither is held across a write that can block.
    fn log(&self, msg: &str) {
        {
            let mut ring = lock(&self.ring);
            if ring.len() >= EVENTS_KEPT {
                ring.pop_front();
            }
            ring.push_back((rfc3339(SystemTime::now()), msg.to_string()));
        }
        let queued = match lock(&self.out).as_ref() {
            Some(tx) => !matches!(tx.try_send(msg.to_string()), Err(TrySendError::Disconnected(_))),
            None => false,
        };
        if !queued {
            eprintln!("diagbuddy: {msg}");
        }
    }

    /// Write out every queued line and stop the writer. Later lines go to stderr directly.
    fn finish(&self) {
        drop(lock(&self.out).take());
        if let Some(h) = lock(&self.writer).take() {
            let _ = h.join();
        }
    }

    fn to_json(&self) -> Value {
        lock(&self.ring).iter().map(|(time, msg)| json!({"time": time, "msg": msg})).collect()
    }
}

/// `run` can return early (a listen address that cannot be bound) after lines were queued;
/// they must still reach stderr before the process exits.
impl Drop for Events {
    fn drop(&mut self) {
        self.finish();
    }
}

// ---- retry delay --------------------------------------------------------------------

/// The wait before the next registration attempt.
struct Backoff {
    base: Duration,
    min_session: Duration,
    delay: Duration,
}

impl Backoff {
    fn new(base: Duration, min_session: Duration) -> Backoff {
        Backoff { base, min_session, delay: base.min(RETRY_CAP) }
    }

    /// An attempt failed: wait the current delay, and double it for next time (capped).
    fn after_failure(&mut self) -> Duration {
        let wait = self.delay;
        self.delay = (wait * 2).min(RETRY_CAP);
        wait
    }

    /// A session that lived `lived` was lost. Only a long-lived one starts over from the
    /// base; a short one is treated as a failure.
    fn after_loss(&mut self, lived: Duration) -> Duration {
        if lived >= self.min_session {
            self.delay = self.base.min(RETRY_CAP);
        }
        self.after_failure()
    }
}

// ---- shared state -------------------------------------------------------------------

struct Session {
    transport: Transport,
    client_id: i32,
    gen: u64,
    /// When it registered.
    since: Instant,
}

impl Session {
    /// Unblock this session's reader. The router socket can be shut down; a `/dev/diag`
    /// reader ends at its next read and then sees it is stale.
    fn close(&self) {
        if let Transport::Router(s) = &self.transport {
            let _ = s.shutdown(Shutdown::Both);
        }
    }
}

struct Core {
    session: Option<Session>,
    /// Failed registration attempts and lost sessions, daemon lifetime.
    failures: u64,
    /// Successful registrations, daemon lifetime.
    registrations: u64,
    /// Client requests answered, all clients (daemon lifetime).
    requests: u64,
    backoff: Backoff,
    next_attempt: Instant,
    /// Codes subscribed on the live session (empty without one).
    subscribed: BTreeSet<u16>,
    /// Per attached client, by id: its responder and the codes it was last granted.
    clients: BTreeMap<u64, ClientState>,
    /// Requests refused for asking for more than [`MAX_SUBSCRIBED_CODES`], daemon lifetime.
    over_cap: u64,
    /// Refusals and counters of clients that have gone, for the daemon's lifetime.
    refused_past: BTreeMap<u8, u64>,
    counters_past: Counters,
}

/// One attached client's half of the DIAG conversation.
#[derive(Default)]
struct ClientState {
    responder: Responder,
    /// Codes its masks ask for, as last accepted (the union with everyone else's fits the cap).
    codes: BTreeSet<u16>,
    requests: u64,
    over_cap: u64,
    /// Logged as over the cap (once per client).
    over_cap_logged: bool,
}

/// An attached client as the reader sees it: where to send, and which codes it wants.
struct Attached {
    client: Client,
    codes: BTreeSet<u16>,
}

struct Shared {
    core: Mutex<Core>,
    /// Attached clients by id (ids count up from 1 and are never reused).
    clients: Mutex<BTreeMap<u64, Attached>>,
    /// Connections the listener refused because `max_clients` were attached (lifetime).
    refused_connections: AtomicU64,
    max_clients: usize,
    /// Generation of the live DCI session, 0 for none. Readers of older ones stand down.
    dci_gen: AtomicU64,
    records: AtomicU64,
    unparsed: AtomicU64,
    /// Something in the status changed; rewrite it soon. Frames sent to the client do not
    /// mark it; `client.sent` is current to within one periodic rewrite.
    dirty: AtomicBool,
    events: Events,
    defaults: BTreeSet<u16>,
    outer: Outer,
}

impl Shared {
    fn mark(&self) {
        self.dirty.store(true, Ordering::Release);
    }
}

impl Core {
    /// Every code some attached client wants, plus the defaults; `except` leaves one client out.
    fn union(&self, sh: &Shared, except: Option<u64>) -> BTreeSet<u16> {
        let mut want = sh.defaults.clone();
        for (_, st) in self.clients.iter().filter(|(&id, _)| Some(id) != except) {
            want.extend(&st.codes);
        }
        want
    }

    /// Client `id` now wants `codes`. Applied when the new union (every other client's codes,
    /// the defaults and these) fits under [`MAX_SUBSCRIBED_CODES`]; otherwise this client's
    /// codes and the subscription are left exactly as they were and the refusal counted (the
    /// client's DIAG reply is unaffected).
    fn want_client_codes(&mut self, sh: &Shared, id: u64, codes: BTreeSet<u16>) {
        let mut total = self.union(sh, Some(id));
        total.extend(&codes);
        let total = total.len();
        let Some(st) = self.clients.get_mut(&id) else { return };
        if total > MAX_SUBSCRIBED_CODES {
            self.over_cap += 1;
            st.over_cap += 1;
            sh.mark();
            if !st.over_cap_logged {
                st.over_cap_logged = true;
                sh.events.log(&format!(
                    "client {id}: {} log codes would make the subscription {total}, over the cap \
                     of {MAX_SUBSCRIBED_CODES}; left as it was",
                    codes.len()
                ));
            }
            return;
        }
        st.codes = codes.clone();
        // The filter changes before the subscription does, so the first record of a code
        // this client just asked for is never discarded on its way in.
        if let Some(a) = lock(&sh.clients).get_mut(&id) {
            a.codes = codes;
        }
        self.sync(sh);
    }

    /// Bring the DCI subscription to the union of the clients' codes and the defaults: one
    /// unsubscribe for codes leaving, one subscribe for codes entering. A default code, or one
    /// another client still wants, never leaves.
    fn sync(&mut self, sh: &Shared) {
        let want = self.union(sh, None);
        let Some(s) = self.session.as_mut() else { return };
        let leaving: Vec<u16> = self.subscribed.difference(&want).copied().collect();
        let entering: Vec<u16> = want.difference(&self.subscribed).copied().collect();
        let mut result = Ok(());
        if !leaving.is_empty() {
            result = s.transport.send(&log_mask_msg(s.client_id, false, &leaving));
            if result.is_ok() {
                leaving.iter().for_each(|c| {
                    self.subscribed.remove(c);
                });
            }
        }
        if result.is_ok() && !entering.is_empty() {
            result = s.transport.send(&log_mask_msg(s.client_id, true, &entering));
            if result.is_ok() {
                self.subscribed.extend(&entering);
            }
        }
        if !leaving.is_empty() || !entering.is_empty() {
            sh.mark();
        }
        if let Err(e) = result {
            self.lose(sh, &format!("subscription write failed: {e}"));
        }
    }

    /// On request: unsubscribe and deregister the live session as on exit, then register
    /// again at the next tick. Not a loss: neither `failures` nor the backoff moves.
    fn reregister(&mut self, sh: &Shared) {
        let Some(mut s) = self.session.take() else {
            sh.events.log("re-registration requested while not registered; nothing to drop");
            return;
        };
        sh.dci_gen.store(0, Ordering::Release);
        let all: Vec<u16> = self.subscribed.iter().copied().collect();
        if !all.is_empty() {
            if let Err(e) = s.transport.send(&log_mask_msg(s.client_id, false, &all)) {
                sh.events.log(&format!("unsubscribe before re-registration: {e}"));
            }
        }
        if let Err(e) = s.transport.deregister(s.client_id) {
            sh.events.log(&format!("deregister before re-registration: {e}"));
        }
        s.close();
        self.subscribed.clear();
        self.next_attempt = Instant::now();
        sh.mark();
        sh.events.log(&format!(
            "re-registration requested: client {} deregistered after {:.0?}; registering again",
            s.client_id,
            s.since.elapsed()
        ));
    }

    /// Drop the session and schedule a retry.
    fn lose(&mut self, sh: &Shared, why: &str) {
        let lived = match self.session.take() {
            Some(s) => {
                sh.dci_gen.store(0, Ordering::Release);
                s.close();
                s.since.elapsed()
            }
            None => Duration::ZERO,
        };
        self.subscribed.clear();
        self.failures += 1;
        let wait = self.backoff.after_loss(lived);
        self.next_attempt = Instant::now() + wait;
        sh.mark();
        sh.events.log(&format!("DCI lost after {lived:.0?} ({why}); retrying in {wait:?}"));
    }

    /// Forget client `id`: keep its counters for the status file, and drop the codes no other
    /// client (and no default) still wants.
    fn retire_client(&mut self, sh: &Shared, id: u64, client: &Client) {
        let st = self.clients.remove(&id).unwrap_or_default();
        for (&cmd, &n) in st.responder.refused() {
            *self.refused_past.entry(cmd).or_insert(0) += n;
        }
        let c = client.counters();
        sh.events.log(&format!(
            "client {id} detached: requests {}, sent {}, dropped {}, bad_requests {}; {} attached",
            st.requests,
            c.sent,
            c.dropped,
            c.bad_requests,
            self.clients.len()
        ));
        self.counters_past.sent += c.sent;
        self.counters_past.dropped += c.dropped;
        self.counters_past.bad_requests += c.bad_requests;
        self.sync(sh);
        sh.mark();
    }
}

// ---- DCI ----------------------------------------------------------------------------

fn open_transport(kind: ProfileTransport, router_addr: Option<&[u8]>) -> io::Result<Transport> {
    match (router_addr, kind) {
        (Some(name), _) => Transport::open_router_at(name),
        (None, ProfileTransport::DiagRouter) => Transport::open_router_at(&ROUTER_NAME),
        // `run` refuses both before getting here.
        (None, t) => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!("no DCI over the {} transport", transport_name(t)),
        )),
    }
}

/// One registration attempt; on success the reader is running and the subscription is set.
fn attempt(sh: &Arc<Shared>, kind: ProfileTransport, router_addr: Option<&[u8]>, next_gen: &mut u64) {
    let connected = open_transport(kind, router_addr).and_then(|mut t| {
        let s = t.handshake()?;
        let reader = t.reader()?;
        Ok((t, s, reader))
    });
    let mut core = lock(&sh.core);
    match connected {
        Ok((transport, s, reader)) => {
            *next_gen += 1;
            let gen = *next_gen;
            sh.events.log(&format!("DCI registered as client {} on the {}", s.client_id, transport.name()));
            core.session = Some(Session { transport, client_id: s.client_id, gen, since: Instant::now() });
            sh.dci_gen.store(gen, Ordering::Release);
            core.registrations += 1;
            core.subscribed.clear();
            sh.mark();
            let r = sh.clone();
            let spawned = thread::Builder::new()
                .name("dci-reader".into())
                .spawn(move || read_loop(&r, reader, gen));
            match spawned {
                Ok(_) => core.sync(sh),
                Err(e) => core.lose(sh, &format!("no reader thread: {e}")),
            }
        }
        Err(e) => {
            core.failures += 1;
            let wait = core.backoff.after_failure();
            core.next_attempt = Instant::now() + wait;
            sh.events.log(&format!("DCI registration failed: {e}; retrying in {wait:?}"));
            sh.mark();
        }
    }
}

fn read_loop(sh: &Shared, mut reader: Box<dyn Read + Send>, gen: u64) {
    let mut buf = vec![0u8; READ_BUF];
    let why = loop {
        let n = match reader.read(&mut buf) {
            Ok(0) => break "end of stream".to_string(),
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => break format!("read error: {e}"),
        };
        if sh.dci_gen.load(Ordering::Acquire) != gen {
            return; // a newer session (or none) owns the stream now
        }
        let Some(p) = parse_data(&buf[..n]) else { continue };
        sh.records.fetch_add(p.records.len() as u64, Ordering::Relaxed);
        sh.unparsed.fetch_add(p.unparsed_bytes as u64, Ordering::Relaxed);
        // Each record goes to the clients that asked for its code; one nobody asked for (no
        // client, or only a default code) is counted above and discarded here.
        let clients = lock(&sh.clients);
        for r in &p.records {
            let mut wanting = clients.values().filter(|a| a.codes.contains(&r.code)).peekable();
            if wanting.peek().is_none() {
                continue;
            }
            let frame = frame_log(r.log, r.header.as_ref(), sh.outer);
            for a in wanting {
                a.client.send(frame.clone());
            }
        }
    };
    let mut core = lock(&sh.core);
    if core.session.as_ref().is_some_and(|s| s.gen == gen) {
        core.lose(sh, &why);
    }
}

// ---- client -------------------------------------------------------------------------

fn install_client(sh: &Arc<Shared>, client: Client, id: u64) {
    let requests = client.requests();
    {
        let mut core = lock(&sh.core);
        core.clients.insert(id, ClientState::default());
        let mut clients = lock(&sh.clients);
        clients.insert(id, Attached { client, codes: BTreeSet::new() });
        sh.events.log(&format!("client {id} attached; {} attached", clients.len()));
        sh.mark();
    }
    let s = sh.clone();
    let spawned = thread::Builder::new()
        .name("client-requests".into())
        .spawn(move || serve_client(&s, requests, id));
    if let Err(e) = spawned {
        sh.events.log(&format!("no request thread for client {id}: {e}"));
        detach_client(sh, id);
    }
}

fn serve_client(sh: &Shared, requests: impl Iterator<Item = Vec<u8>>, id: u64) {
    for req in requests {
        let response = {
            let mut core = lock(&sh.core);
            let Some(st) = core.clients.get_mut(&id) else {
                return; // already retired (daemon stopping)
            };
            st.requests += 1;
            let reply = st.responder.handle(&req);
            core.requests += 1;
            sh.mark();
            if let Some(codes) = reply.subscription {
                core.want_client_codes(sh, id, codes);
            }
            reply.response
        };
        // The DCI change is written before the reply, so a client that has read the reply
        // knows the stack has been asked.
        let mut frame = Vec::with_capacity(response.len() + 8);
        hdlc_encode(&response, &mut frame);
        if let Some(a) = lock(&sh.clients).get(&id) {
            a.client.send(frame);
        }
    }
    detach_client(sh, id);
}

fn detach_client(sh: &Shared, id: u64) {
    let gone = {
        let mut core = lock(&sh.core);
        // The `clients` lock is released before retiring: that writes to DCI, and the reader
        // needs the lock to keep draining the stack meanwhile.
        let Some(gone) = lock(&sh.clients).remove(&id) else { return };
        core.retire_client(sh, id, &gone.client);
        gone
    };
    drop(gone); // joins the writer; done outside the locks
}

// ---- stock consumers ----------------------------------------------------------------

struct Consumer {
    name: String,
    pidof: String,
    pid_at_start: Option<u32>,
}

/// Pids under `proc_root` whose `comm` is exactly `comm`, ascending.
fn pids_named(proc_root: &Path, comm: &str) -> Vec<u32> {
    let Ok(rd) = fs::read_dir(proc_root) else { return Vec::new() };
    let mut pids: Vec<u32> = rd
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
        .filter(|pid| {
            fs::read_to_string(proc_root.join(pid.to_string()).join("comm"))
                .is_ok_and(|c| c.trim_end_matches('\n') == comm)
        })
        .collect();
    pids.sort_unstable();
    pids
}

/// `present` when the pid seen at start still runs that program, `restarted` when another
/// pid does (including one that was not running at start), `absent` when none does.
fn consumer_status(c: &Consumer, proc_root: &Path) -> Value {
    let now = pids_named(proc_root, &c.pidof);
    let (pid_now, state) = match c.pid_at_start {
        Some(p) if now.contains(&p) => (Some(p), "present"),
        _ => match now.first() {
            Some(&p) => (Some(p), "restarted"),
            None => (None, "absent"),
        },
    };
    json!({"name": c.name, "pid_at_start": c.pid_at_start, "pid_now": pid_now, "state": state})
}

// ---- status file --------------------------------------------------------------------

/// `YYYY-MM-DDTHH:MM:SSZ` for a time at or after the epoch.
/// Wall-clock start time of a process that has been up `up`, read against the clock `now`.
/// Taking the wall clock once at start was wrong for the daemon's whole life when the clock
/// was set after it started (no NTP or NITZ yet at boot): `since` read 1970 or a stale
/// build date. Derived at every status write, it follows the clock once it is set.
/// A clock still behind `up` saturates to the epoch.
pub fn started_at(now: SystemTime, up: Duration) -> SystemTime {
    now.checked_sub(up).filter(|t| *t >= UNIX_EPOCH).unwrap_or(UNIX_EPOCH)
}

pub fn rfc3339(t: SystemTime) -> String {
    let secs = t.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let (days, rem) = ((secs / 86_400) as i64, secs % 86_400);
    // Civil date from days since 1970-01-01 (H. Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem / 60 % 60, rem % 60)
}

/// The inverse of [`rfc3339`], for exactly the form it writes.
pub fn parse_rfc3339(s: &str) -> Option<SystemTime> {
    let b = s.as_bytes();
    let shape = b.len() == 20
        && b.iter().enumerate().all(|(i, &c)| match i {
            4 | 7 => c == b'-',
            10 => c == b'T',
            13 | 16 => c == b':',
            19 => c == b'Z',
            _ => c.is_ascii_digit(),
        });
    if !shape {
        return None;
    }
    let num = |r: std::ops::Range<usize>| s[r].parse::<i64>().ok();
    let (y, m, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (hh, mm, ss) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || hh > 23 || mm > 59 || ss > 59 {
        return None;
    }
    // Days since 1970-01-01 from a civil date (H. Hinnant's algorithm).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = u64::try_from(days * 86_400 + hh * 3600 + mm * 60 + ss).ok()?;
    Some(UNIX_EPOCH + Duration::from_secs(secs))
}

struct StatusInfo {
    /// Monotonic start time. `since` is derived from it at every write (see [`started_at`]).
    started: Instant,
    host: String,
    modem: String,
    transport: &'static str,
    listen: String,
    consumers: Vec<Consumer>,
    proc_root: PathBuf,
}

fn status_json(sh: &Shared, info: &StatusInfo) -> Value {
    // Gathered before the locks: the `/proc` scan takes a few milliseconds.
    let consumers: Vec<Value> =
        info.consumers.iter().map(|c| consumer_status(c, &info.proc_root)).collect();
    let events = sh.events.to_json();
    let core = lock(&sh.core);
    let attached = lock(&sh.clients);
    let refused_json = |m: &BTreeMap<u8, u64>| -> Map<String, Value> {
        m.iter().map(|(cmd, n)| (format!("0x{cmd:02x}"), json!(n))).collect()
    };
    let codes_json =
        |s: &BTreeSet<u16>| s.iter().map(|c| format!("0x{c:04X}")).collect::<Vec<_>>();
    let mut refused = core.refused_past.clone();
    let mut now = Counters::default();
    let mut per_client = Vec::new();
    for (id, a) in attached.iter() {
        let c = a.client.counters();
        now.sent += c.sent;
        now.dropped += c.dropped;
        now.bad_requests += c.bad_requests;
        let st = core.clients.get(id);
        let own_refused = st.map(|s| s.responder.refused().clone()).unwrap_or_default();
        for (&cmd, &n) in &own_refused {
            *refused.entry(cmd).or_insert(0) += n;
        }
        per_client.push(json!({
            "id": id,
            "codes": codes_json(&a.codes),
            "sent": c.sent,
            "dropped": c.dropped,
            "bad_requests": c.bad_requests,
            "requests": st.map_or(0, |s| s.requests),
            "refused_commands": refused_json(&own_refused),
            "over_cap": st.map_or(0, |s| s.over_cap),
        }));
    }
    let refused = refused_json(&refused);
    let past = core.counters_past;
    json!({
        "pid": std::process::id(),
        "since": rfc3339(started_at(SystemTime::now(), info.started.elapsed())),
        "updated": rfc3339(SystemTime::now()),
        "profile": {"host": info.host, "modem": info.modem},
        "dci": if core.session.is_some() { "registered" } else { "retrying" },
        "dci_registrations": core.registrations,
        "dci_failures": core.failures,
        "client_id": core.session.as_ref().map(|s| s.client_id),
        "transport": info.transport,
        "listen": info.listen,
        "client": {
            "attached": attached.len(),
            "max": sh.max_clients,
            "refused_connections": sh.refused_connections.load(Ordering::Relaxed),
            "sent": past.sent + now.sent,
            "requests": core.requests,
            "dropped": past.dropped + now.dropped,
            "bad_requests": past.bad_requests + now.bad_requests,
            "refused_commands": refused,
            "over_cap": core.over_cap,
        },
        "clients": per_client,
        "subscribed": codes_json(&core.subscribed),
        "records": sh.records.load(Ordering::Relaxed),
        "unparsed_bytes": sh.unparsed.load(Ordering::Relaxed),
        "stock_consumers": consumers,
        "events": events,
    })
}

/// Write via a temp file in the same directory and rename, so a reader never sees half.
fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    fs::create_dir_all(dir)?;
    let name = path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default();
    let tmp = dir.join(format!(".{name}.tmp"));
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path)
}

fn write_status(sh: &Shared, info: &StatusInfo, path: &Path) -> io::Result<()> {
    let mut text = serde_json::to_vec_pretty(&status_json(sh, info)).map_err(io::Error::other)?;
    text.push(b'\n');
    write_atomic(path, &text)
}

// ---- run ----------------------------------------------------------------------------

fn parse_code(c: &str) -> Option<u16> {
    u16::from_str_radix(c.strip_prefix("0x")?, 16).ok()
}

fn transport_name(t: ProfileTransport) -> &'static str {
    match t {
        ProfileTransport::DiagRouter => "diag-router",
        ProfileTransport::KernelChar => "kernel-char",
        ProfileTransport::Serial => "serial",
    }
}

/// Run until `opts.stop` is requested, then unsubscribe, deregister, remove the status file
/// and return `Ok(())`. Errors only before anything is registered: a bad or unsupported
/// profile, a listen address that cannot be bound, a status file that cannot be written.
pub fn run(profile: Profile, opts: Opts) -> io::Result<()> {
    let invalid = |m: String| io::Error::new(io::ErrorKind::InvalidInput, m);
    profile.check().map_err(invalid)?;
    let d = &profile.diag;
    if d.session != SessionMode::Dci {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "diag.session \"exclusive\" is not built yet (serial and exclusive sessions arrive \
             in phase 3); only \"dci\" runs",
        ));
    }
    if d.transport == ProfileTransport::KernelChar {
        // Its loss path would leave the old registration open (the reader keeps the fd).
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "diag.transport \"kernel-char\" is not built yet (a /dev/diag session cannot yet be \
             torn down and retried cleanly); only \"diag-router\" runs",
        ));
    }
    let defaults: BTreeSet<u16> = d
        .default_codes
        .iter()
        .map(|c| parse_code(c).ok_or_else(|| invalid(format!("bad default code {c}"))))
        .collect::<io::Result<_>>()?;

    let events = Events::new();
    let consumers = d
        .stock_consumers
        .iter()
        .map(|c| {
            let pid = pids_named(&opts.proc_root, &c.pidof).first().copied();
            if pid.is_none() {
                events.log(&format!("stock consumer {} ({}) is not running", c.name, c.pidof));
            }
            Consumer { name: c.name.clone(), pidof: c.pidof.clone(), pid_at_start: pid }
        })
        .collect();

    let listener = Listener::bind(&profile.stream.listen, profile.stream.max_clients)?;
    let info = StatusInfo {
        started: Instant::now(),
        host: opts.host.clone(),
        modem: opts.modem.clone(),
        transport: transport_name(d.transport),
        listen: listener.local_addr(),
        consumers,
        proc_root: opts.proc_root.clone(),
    };
    let sh = Arc::new(Shared {
        core: Mutex::new(Core {
            session: None,
            failures: 0,
            registrations: 0,
            requests: 0,
            backoff: Backoff::new(opts.retry_base, opts.min_session),
            next_attempt: Instant::now(),
            subscribed: BTreeSet::new(),
            clients: BTreeMap::new(),
            over_cap: 0,
            refused_past: BTreeMap::new(),
            counters_past: Counters::default(),
        }),
        clients: Mutex::new(BTreeMap::new()),
        refused_connections: AtomicU64::new(0),
        max_clients: listener.max_clients(),
        dci_gen: AtomicU64::new(0),
        records: AtomicU64::new(0),
        unparsed: AtomicU64::new(0),
        dirty: AtomicBool::new(false),
        events,
        defaults,
        outer: if d.multi_radio_header { Outer::MultiRadio } else { Outer::None },
    });
    // A status file that cannot be written is a setup error, reported before registering.
    write_status(&sh, &info, &opts.status_file)?;
    sh.events.log(&format!("serving {} for {}/{}", info.listen, info.host, info.modem));

    let mut dci_gen = 0u64;
    let mut client_id = 0u64;
    let mut refused_seen = 0u64;
    let mut last_status = Instant::now();
    let mut result = Ok(());
    while !opts.stop.requested() {
        if opts.reregister.take() {
            lock(&sh.core).reregister(&sh);
        }
        let due = {
            let core = lock(&sh.core);
            core.session.is_none() && Instant::now() >= core.next_attempt
        };
        if due {
            attempt(&sh, d.transport, opts.router_addr.as_deref(), &mut dci_gen);
        }
        let refused = listener.refused();
        if refused != refused_seen {
            sh.refused_connections.store(refused, Ordering::Relaxed);
            sh.events.log(&format!(
                "refused {} stream connection(s): {} clients attached, the limit (lifetime {refused})",
                refused - refused_seen,
                listener.max_clients()
            ));
            refused_seen = refused;
            sh.mark();
        }
        match listener.accept_timeout(TICK) {
            Ok(Some(client)) => {
                client_id += 1;
                install_client(&sh, client, client_id);
            }
            Ok(None) => {}
            Err(e) => {
                result = Err(e);
                break;
            }
        }
        let since = last_status.elapsed();
        // Peek, then clear only when writing: a mark that arrives inside the gap is kept.
        if status_due(since, sh.dirty.load(Ordering::Acquire), opts.status_interval, opts.status_min_gap) {
            sh.dirty.store(false, Ordering::Release);
            last_status = Instant::now();
            if let Err(e) = write_status(&sh, &info, &opts.status_file) {
                sh.events.log(&format!("status file {}: {e}", opts.status_file.display()));
            }
        }
    }
    shutdown(&sh, listener, &opts.status_file);
    result
}

/// Whether the status file is due: always after `interval`, and after `min_gap` when
/// something marked it dirty.
fn status_due(since_write: Duration, dirty: bool, interval: Duration, min_gap: Duration) -> bool {
    since_write >= interval || (dirty && since_write >= min_gap)
}

/// Unsubscribe everything, deregister, close every client and remove the status file.
fn shutdown(sh: &Shared, listener: Listener, status_file: &Path) {
    let clients = {
        let mut core = lock(&sh.core);
        if let Some(mut s) = core.session.take() {
            sh.dci_gen.store(0, Ordering::Release);
            let all: Vec<u16> = core.subscribed.iter().copied().collect();
            if !all.is_empty() {
                if let Err(e) = s.transport.send(&log_mask_msg(s.client_id, false, &all)) {
                    sh.events.log(&format!("unsubscribe on exit: {e}"));
                }
            }
            if let Err(e) = s.transport.deregister(s.client_id) {
                sh.events.log(&format!("deregister on exit: {e}"));
            }
            s.close();
            core.subscribed.clear();
        }
        // Request threads still running find their client retired and stop.
        core.clients.clear();
        std::mem::take(&mut *lock(&sh.clients))
    };
    drop(clients);
    drop(listener);
    match fs::remove_file(status_file) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => {
            sh.events.log(&format!("removing {}: {e}", status_file.display()))
        }
        _ => {}
    }
    sh.events.log("stopped");
    sh.events.finish();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_formats_known_instants() {
        let at = |s| rfc3339(UNIX_EPOCH + Duration::from_secs(s));
        assert_eq!(at(0), "1970-01-01T00:00:00Z");
        assert_eq!(at(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(at(1_700_000_000), "2023-11-14T22:13:20Z");
        assert_eq!(at(4_102_444_799), "2099-12-31T23:59:59Z");
    }

    const MS: fn(u64) -> Duration = Duration::from_millis;

    #[test]
    fn since_follows_a_clock_set_after_start() {
        let s = |secs| UNIX_EPOCH + Duration::from_secs(secs);
        let up = Duration::from_secs(100);
        // 2026-11-18T12:00:00Z once NTP has set the clock, 100 s after start
        assert_eq!(rfc3339(started_at(s(1_795_003_200), up)), "2026-11-18T11:58:20Z");
        // the same process read earlier, while the clock still said 1970 + 150 s
        assert_eq!(started_at(s(150), up), s(50));
        // a clock behind the uptime saturates rather than wrapping or panicking
        assert_eq!(started_at(s(10), up), UNIX_EPOCH);
    }

    #[test]
    fn a_dirty_status_waits_for_the_minimum_gap() {
        let (iv, gap) = (Duration::from_secs(5), Duration::from_secs(1));
        assert!(!status_due(MS(50), true, iv, gap), "a flood must not rewrite every tick");
        assert!(!status_due(MS(999), true, iv, gap));
        assert!(status_due(MS(1000), true, iv, gap));
    }

    #[test]
    fn a_clean_status_is_rewritten_only_on_the_interval() {
        let (iv, gap) = (Duration::from_secs(5), Duration::from_secs(1));
        assert!(!status_due(MS(1500), false, iv, gap));
        assert!(status_due(MS(5000), false, iv, gap));
    }

    #[test]
    fn a_flood_rewrites_at_most_once_per_gap() {
        // 20 ticks a second for 10 s, every tick marks the status dirty.
        let (iv, gap) = (STATUS_INTERVAL, STATUS_MIN_GAP);
        let (mut since, mut writes) = (Duration::ZERO, 0);
        for _ in 0..200 {
            since += MS(50);
            if status_due(since, true, iv, gap) {
                writes += 1;
                since = Duration::ZERO;
            }
        }
        assert_eq!(writes, 10, "one rewrite per {gap:?}, not one per request");
    }

    fn backoff() -> Backoff {
        Backoff::new(Duration::from_secs(1), Duration::from_secs(60))
    }

    #[test]
    fn failed_attempts_wait_ever_longer_up_to_the_cap() {
        let mut b = backoff();
        let waits: Vec<u64> = (0..8).map(|_| b.after_failure().as_secs()).collect();
        assert_eq!(waits, [1, 2, 4, 8, 16, 30, 30, 30]);
    }

    #[test]
    fn a_short_session_does_not_reset_the_wait() {
        let mut b = backoff();
        b.after_failure();
        b.after_failure();
        // Registered, then lost after 10 s: no reset, the wait keeps growing.
        assert_eq!(b.after_loss(Duration::from_secs(10)), Duration::from_secs(4));
        assert_eq!(b.after_loss(MS(1)), Duration::from_secs(8));
        for _ in 0..10 {
            b.after_loss(MS(1));
        }
        assert_eq!(b.after_loss(MS(1)), RETRY_CAP);
    }

    #[test]
    fn a_long_lived_session_resets_the_wait_to_the_base() {
        let mut b = backoff();
        for _ in 0..10 {
            b.after_failure();
        }
        assert_eq!(b.after_loss(Duration::from_secs(60)), Duration::from_secs(1));
        assert_eq!(b.after_failure(), Duration::from_secs(2));
        assert_eq!(b.after_loss(Duration::from_secs(3600)), Duration::from_secs(1));
    }

    #[test]
    fn rfc3339_parses_back_what_it_writes() {
        for secs in [0u64, 951_782_400, 1_700_000_000, 4_102_444_799, 1_234_567_890] {
            let t = UNIX_EPOCH + Duration::from_secs(secs);
            assert_eq!(parse_rfc3339(&rfc3339(t)), Some(t), "{secs}");
        }
        for bad in ["", "2023-11-14 22:13:20Z", "2023-11-14T22:13:20", "2023-13-14T22:13:20Z", "yesterday"] {
            assert_eq!(parse_rfc3339(bad), None, "{bad}");
        }
    }

    #[test]
    fn the_event_ring_keeps_the_newest_lines() {
        let ev = Events::new();
        for i in 0..EVENTS_KEPT + 5 {
            ev.log(&format!("line {i}"));
        }
        let j = ev.to_json();
        let msgs: Vec<&str> = j.as_array().unwrap().iter().map(|e| e["msg"].as_str().unwrap()).collect();
        assert_eq!(msgs.len(), EVENTS_KEPT);
        assert_eq!(msgs[0], "line 5");
        assert_eq!(msgs[EVENTS_KEPT - 1], format!("line {}", EVENTS_KEPT + 4));
    }

    /// A sink that blocks every write until released, and records what it was given.
    struct GatedSink {
        gate: std::sync::mpsc::Receiver<()>,
        got: Arc<Mutex<Vec<u8>>>,
    }

    impl Write for GatedSink {
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            let _ = self.gate.recv();
            lock(&self.got).extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_stalled_stderr_never_blocks_the_caller() {
        // procd's stderr pipe can stall. log() is called with the core lock held, so a
        // blocking write there would stall the supervisor and every request.
        let (release, gate) = std::sync::mpsc::channel();
        let got = Arc::new(Mutex::new(Vec::new()));
        let ev = Events::with_sink(Box::new(GatedSink { gate, got: got.clone() }));
        let t0 = Instant::now();
        for i in 0..STDERR_QUEUE * 2 {
            ev.log(&format!("line {i}"));
        }
        assert!(t0.elapsed() < Duration::from_secs(1), "log() blocked on the sink: {:?}", t0.elapsed());
        // The ring is unaffected by the stall.
        assert_eq!(ev.to_json().as_array().unwrap().len(), EVENTS_KEPT);
        drop(release); // the gate now returns at once
        ev.finish();
        assert!(!lock(&got).is_empty());
    }

    #[test]
    fn finish_delivers_every_queued_line_in_order() {
        let (release, gate) = std::sync::mpsc::channel();
        let got = Arc::new(Mutex::new(Vec::new()));
        let ev = Events::with_sink(Box::new(GatedSink { gate, got: got.clone() }));
        drop(release);
        for i in 0..5 {
            ev.log(&format!("line {i}"));
        }
        ev.finish();
        let text = String::from_utf8(lock(&got).clone()).unwrap();
        let want: String = (0..5).map(|i| format!("diagbuddy: line {i}\n")).collect();
        assert_eq!(text, want);
    }

    #[test]
    fn dropping_the_log_delivers_what_was_queued() {
        let (release, gate) = std::sync::mpsc::channel();
        let got = Arc::new(Mutex::new(Vec::new()));
        let ev = Events::with_sink(Box::new(GatedSink { gate, got: got.clone() }));
        drop(release);
        ev.log("early exit");
        drop(ev);
        assert_eq!(String::from_utf8(lock(&got).clone()).unwrap(), "diagbuddy: early exit\n");
    }

    #[test]
    fn codes_parse_case_insensitively() {
        assert_eq!(parse_code("0xB0C0"), Some(0xB0C0));
        assert_eq!(parse_code("0xb0c0"), Some(0xB0C0));
        assert_eq!(parse_code("B0C0"), None);
    }
}
