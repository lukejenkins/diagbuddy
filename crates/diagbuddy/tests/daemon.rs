// SPDX-License-Identifier: Apache-2.0
//! The daemon end to end: `daemon::run` on a thread against a `FakeRouter`, with a client on
//! loopback TCP.
#[path = "../../dcicore/tests/common/fake_router.rs"]
mod fake_router;

use dcicore::frame::{hdlc_decode, hdlc_encode};
use diagbuddy::daemon::{self, Opts, Reregister, Stop};
use diagbuddy::profile::Profile;
use fake_router::FakeRouter;
use serde_json::{json, Value};
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const WAIT: Duration = Duration::from_secs(5);

struct TempDir(PathBuf);
impl TempDir {
    fn new(tag: &str) -> TempDir {
        let p = std::env::temp_dir().join(format!("dbd-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        TempDir(p)
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A unique abstract socket name per test and process.
fn router_name(tag: &str) -> Vec<u8> {
    format!("diagbuddy-test-{}-{tag}", std::process::id()).into_bytes()
}

fn profile(diag: Value) -> Profile {
    let mut d = json!({"transport": "diag-router", "session": "dci"});
    diagbuddy::profile::merge(&mut d, &diag);
    serde_json::from_value(json!({
        "spec_version": 1,
        "diag": d,
        "storage": {"recordings_path": "/nonexistent/recordings"},
        "stream": {"listen": "tcp:127.0.0.1:0"}
    }))
    .unwrap()
}

/// Poll `f` until it returns `Some`, or panic after `limit`.
fn wait_for<T>(limit: Duration, what: &str, mut f: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + limit;
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

struct Harness {
    router: Option<FakeRouter>,
    name: Vec<u8>,
    _dir: TempDir,
    status: PathBuf,
    stop: Stop,
    daemon: Option<JoinHandle<io::Result<()>>>,
    /// Id of the last client `connect` attached.
    next_id: std::cell::Cell<u64>,
}

impl Harness {
    fn start(tag: &str, diag: Value) -> Harness {
        Harness::start_with(tag, diag, |_| {})
    }

    fn start_with(tag: &str, diag: Value, tweak: impl FnOnce(&mut Opts)) -> Harness {
        Harness::start_full(tag, profile(diag), tweak)
    }

    fn start_with_profile(tag: &str, p: Profile) -> Harness {
        Harness::start_full(tag, p, |_| {})
    }

    fn start_full(tag: &str, p: Profile, tweak: impl FnOnce(&mut Opts)) -> Harness {
        let name = router_name(tag);
        let router = FakeRouter::start(&name);
        let dir = TempDir::new(tag);
        let status = dir.0.join("run").join("diagbuddy.status.json");
        let mut opts = Opts::new(status.clone());
        opts.router_addr = Some(name.clone());
        opts.host = "test-host".into();
        opts.modem = "test-modem".into();
        opts.retry_base = Duration::from_millis(50);
        opts.status_interval = Duration::from_millis(100);
        opts.proc_root = dir.0.join("no-proc");
        tweak(&mut opts);
        let stop = opts.stop.clone();
        let daemon = Some(std::thread::spawn(move || daemon::run(p, opts)));
        let h = Harness {
            router: Some(router),
            name,
            _dir: dir,
            status,
            stop,
            daemon,
            next_id: std::cell::Cell::new(0),
        };
        h.wait_status("dci registered", |s| s["dci"] == "registered");
        h
    }

    fn router(&self) -> &FakeRouter {
        self.router.as_ref().unwrap()
    }

    fn status(&self) -> Option<Value> {
        let text = std::fs::read_to_string(&self.status).ok()?;
        Some(serde_json::from_str(&text).expect("status file is whole JSON"))
    }

    fn wait_status(&self, what: &str, pred: impl Fn(&Value) -> bool) -> Value {
        self.wait_status_for(WAIT, what, pred)
    }

    fn wait_status_for(&self, limit: Duration, what: &str, pred: impl Fn(&Value) -> bool) -> Value {
        wait_for(limit, what, || self.status().filter(|s| pred(s)))
    }

    fn wait_subscriptions(&self, what: &str, pred: impl Fn(&[(bool, Vec<u16>)]) -> bool) {
        wait_for(WAIT, what, || pred(&self.router().subscriptions()).then_some(()));
    }

    fn addr(&self) -> String {
        let s = self.wait_status("a listen address", |s| s["listen"].is_string());
        s["listen"].as_str().unwrap().strip_prefix("tcp:").unwrap().to_string()
    }

    /// Connect one more client and wait until the daemon lists it. Client ids count up from 1
    /// and are never reused, so waiting for the next id cannot be fooled by a detach that the
    /// status file has not caught up with yet.
    fn connect(&self) -> TcpStream {
        let id = self.next_id.get() + 1;
        self.next_id.set(id);
        let c = TcpStream::connect(self.addr()).unwrap();
        c.set_read_timeout(Some(WAIT)).unwrap();
        self.wait_status(&format!("client {id} attached"), |s| {
            s["clients"].as_array().is_some_and(|cs| cs.iter().any(|c| c["id"] == id))
        });
        c
    }

    fn daemon_alive(&self) -> bool {
        !self.daemon.as_ref().unwrap().is_finished()
    }

    /// Request shutdown (the signal handler's path) and return what `run` returned.
    fn shutdown(&mut self) -> io::Result<()> {
        self.stop.request();
        self.daemon.take().unwrap().join().expect("daemon thread panicked")
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        // Never panic here: a test that already failed must report, not abort.
        if let Some(d) = self.daemon.take() {
            self.stop.request();
            let _ = d.join();
        }
    }
}

fn send_req(c: &mut TcpStream, pkt: &[u8]) {
    let mut f = Vec::new();
    hdlc_encode(pkt, &mut f);
    c.write_all(&f).unwrap();
}

/// Read one HDLC frame and decode it.
fn read_frame(c: &mut TcpStream) -> Vec<u8> {
    let mut raw = Vec::new();
    let mut b = [0u8; 1];
    loop {
        c.read_exact(&mut b).expect("frame from the daemon");
        raw.push(b[0]);
        if b[0] == 0x7E {
            if raw.len() == 1 {
                raw.clear();
                continue;
            }
            return hdlc_decode(&raw).expect("frame with a good CRC");
        }
    }
}

/// LOG_CONFIG op 3: set the mask of equipment id `equip` to exactly `items`.
fn set_mask(equip: u32, items: &[u32]) -> Vec<u8> {
    let last_item = items.iter().max().map_or(0, |m| m + 1);
    let mut req = vec![0x73, 0, 0, 0, 3, 0, 0, 0];
    req.extend(equip.to_le_bytes());
    req.extend(last_item.to_le_bytes());
    let mut mask = vec![0u8; last_item.div_ceil(8) as usize];
    for &i in items {
        mask[(i / 8) as usize] |= 1 << (i % 8);
    }
    req.extend(mask);
    req
}

fn get_mask(equip: u32) -> Vec<u8> {
    let mut req = vec![0x73, 0, 0, 0, 4, 0, 0, 0];
    req.extend(equip.to_le_bytes());
    req
}

/// Client subscribes 0xB0C0 and reads the op 3 echo.
fn subscribe_b0c0(c: &mut TcpStream) {
    let req = set_mask(0xB, &[0xC0]);
    send_req(c, &req);
    let resp = read_frame(c);
    assert_eq!(&resp[..8], &req[..8]);
    assert_eq!(&resp[8..12], &[0, 0, 0, 0], "status 0");
    assert_eq!(&resp[12..], &req[8..]);
}

fn last_sub(subs: &[(bool, Vec<u16>)]) -> Option<(bool, Vec<u16>)> {
    subs.last().cloned()
}

#[test]
fn a_client_mask_becomes_a_dci_subscription_and_logs_come_back_as_hdlc() {
    let h = Harness::start("logs", json!({}));
    let mut c = h.connect();
    subscribe_b0c0(&mut c);
    assert_eq!(last_sub(&h.router().subscriptions()), Some((true, vec![0xB0C0])));
    h.wait_status("subscribed list", |s| s["subscribed"] == json!(["0xB0C0"]));

    let payload = vec![0x01, 0x7E, 0x7D, 0x02];
    h.router().push_logs(&[(0xB0C0, payload.clone())]);
    let pkt = read_frame(&mut c);
    assert_eq!(pkt[0], 0x10, "a bare log packet, no 0x98 header");
    assert_eq!(pkt[1], 0);
    let len = 12 + payload.len();
    assert_eq!(u16::from_le_bytes([pkt[2], pkt[3]]) as usize, len);
    let rec = &pkt[4..];
    assert_eq!(rec.len(), len);
    assert_eq!(u16::from_le_bytes([rec[0], rec[1]]) as usize, len);
    assert_eq!(u16::from_le_bytes([rec[2], rec[3]]), 0xB0C0);
    assert_eq!(&rec[12..], &payload[..]);
    h.wait_status("record counted and sent", |s| s["records"] == 1 && s["client"]["sent"] == 2);
}

#[test]
fn multi_radio_header_is_added_when_the_profile_asks() {
    let h = Harness::start("mrh", json!({"multi_radio_header": true}));
    let mut c = h.connect();
    subscribe_b0c0(&mut c);
    h.router().push_logs(&[(0xB0C0, vec![9])]);
    let pkt = read_frame(&mut c);
    assert_eq!(&pkt[..8], &dcicore::frame::DEFAULT_MULTI_RADIO);
    assert_eq!(pkt[8], 0x10);
}

#[test]
fn client_disconnect_unsubscribes_and_a_new_client_starts_clean() {
    let h = Harness::start("disc", json!({}));
    let mut c = h.connect();
    subscribe_b0c0(&mut c);
    drop(c);
    h.wait_subscriptions("unsubscribe on disconnect", |s| {
        last_sub(s) == Some((false, vec![0xB0C0]))
    });
    h.wait_status("client detached", |s| s["client"]["attached"] == 0 && s["subscribed"] == json!([]));

    let s = h.status().unwrap();
    let msgs = event_msgs(&s);
    let detached = msgs.iter().find(|m| m.starts_with("client 1 detached")).expect("detach line");
    assert!(detached.contains("requests 1") && detached.contains("sent 1"), "{detached}");
    assert!(msgs.iter().any(|m| m.starts_with("client 1 attached")), "{msgs:?}");

    let mut c2 = h.connect();
    send_req(&mut c2, &get_mask(0xB));
    let resp = read_frame(&mut c2);
    let mut want = vec![0x73, 0, 0, 0, 4, 0, 0, 0, 0, 0, 0, 0, 0x0B, 0, 0, 0];
    want.extend(vec![0u8; 0x1000 / 8]);
    assert_eq!(resp, want, "a new client sees an all-zero mask");
    h.wait_status("requests counted across clients", |s| s["client"]["requests"] == 2);
    assert!(h.daemon_alive());
}

#[test]
fn router_loss_is_retried_and_reported() {
    let h = Harness::start("loss", json!({}));
    let mut c = h.connect();
    subscribe_b0c0(&mut c);
    h.router().drop_client();
    h.wait_status_for(Duration::from_secs(3), "dci retrying", |s| s["dci"] == "retrying");
    h.wait_status("dci registered again", |s| s["dci"] == "registered");
    assert_eq!(h.router().registrations().len(), 2);
    let s = h.wait_status("counters", |s| s["dci_registrations"] == 2);
    assert_eq!(s["dci_failures"], 1, "a loss counts as a failure and is not forgotten");
    assert!(event_msgs(&s).iter().any(|m| m.starts_with("DCI lost")), "{s}");
    // The client's codes are subscribed again on the new session.
    h.wait_subscriptions("resubscribed", |s| last_sub(s) == Some((true, vec![0xB0C0])));
    assert_eq!(h.router().subscriptions().iter().filter(|s| s.0).count(), 2);
    // The client stayed attached throughout.
    h.router().push_logs(&[(0xB0C0, vec![1])]);
    assert_eq!(read_frame(&mut c)[0], 0x10);
}

#[test]
fn a_requested_reregistration_deregisters_and_registers_again_from_the_same_pid() {
    let rr = Reregister::default();
    let h = Harness::start_with("rereg", json!({"default_codes": ["0xB0C1"]}), |o| {
        o.reregister = rr.clone();
    });
    let mut c = h.connect();
    subscribe_b0c0(&mut c);
    h.wait_status("first registration", |s| s["dci_registrations"] == 1);
    rr.request();
    let s = h.wait_status("registered again", |s| {
        s["dci"] == "registered" && s["dci_registrations"] == 2
    });
    // A deliberate drop is not a loss: no failure, no backoff.
    assert_eq!(s["dci_failures"], 0, "{s}");
    assert!(
        event_msgs(&s).iter().any(|m| m.starts_with("re-registration requested: client 1 deregistered")),
        "{s}"
    );
    // The old client was unsubscribed and deregistered before the new one registered.
    assert_eq!(h.router().deregistrations(), vec![1]);
    let regs = h.router().registrations();
    assert_eq!(regs.len(), 2);
    let pid = std::process::id().to_le_bytes();
    assert!(regs.iter().all(|r| r[..4] == pid), "both registrations carry this process's pid");
    let subs = h.router().subscriptions();
    assert!(subs.contains(&(false, vec![0xB0C0, 0xB0C1])), "{subs:?}");
    // The client's codes and the defaults are subscribed again on the new session, and the
    // client, attached throughout, still gets records.
    h.wait_subscriptions("resubscribed", |s| last_sub(s) == Some((true, vec![0xB0C0, 0xB0C1])));
    h.router().push_logs(&[(0xB0C0, vec![1])]);
    assert_eq!(read_frame(&mut c)[0], 0x10);
}

#[test]
fn a_reregistration_request_while_not_registered_changes_nothing() {
    let rr = Reregister::default();
    let mut h = Harness::start_with("rereg-idle", json!({}), |o| o.reregister = rr.clone());
    drop(h.router.take());
    h.wait_status("retrying", |s| s["dci"] == "retrying");
    rr.request();
    let s = h.wait_status("noted", |s| {
        event_msgs(s).iter().any(|m| m.starts_with("re-registration requested while not registered"))
    });
    assert_eq!(s["dci"], "retrying", "{s}");
    assert!(h.daemon_alive());
}

#[test]
fn a_router_that_is_gone_is_retried_until_it_returns() {
    let mut h = Harness::start("gone", json!({}));
    drop(h.router.take());
    h.wait_status("connect failing, still retrying", |s| {
        s["dci"] == "retrying" && s["dci_failures"].as_u64() >= Some(2) && s["client_id"].is_null()
    });
    assert!(h.daemon_alive());
    h.router = Some(FakeRouter::start(&h.name));
    h.wait_status("registered with the new router", |s| s["dci"] == "registered");
    assert_eq!(h.router().registrations().len(), 1);
    let s = h.wait_status("counters", |s| s["dci_registrations"] == 2);
    // The loss and at least one failed attempt (waited for above) stay counted.
    assert!(s["dci_failures"].as_u64() >= Some(2), "{s}");
}

#[test]
fn default_codes_are_subscribed_without_a_client() {
    let h = Harness::start("defaults", json!({"default_codes": ["0xB0C1", "0xb0c2"]}));
    h.wait_subscriptions("defaults", |s| s == [(true, vec![0xB0C1, 0xB0C2])]);
    h.wait_status("subscribed list", |s| s["subscribed"] == json!(["0xB0C1", "0xB0C2"]));

    // A client asking for a subset, then nothing, never unsubscribes a default code.
    let mut c = h.connect();
    send_req(&mut c, &set_mask(0xB, &[0xC0, 0xC1]));
    read_frame(&mut c);
    h.wait_subscriptions("client code added", |s| last_sub(s) == Some((true, vec![0xB0C0])));
    send_req(&mut c, &[0x73, 0, 0, 0, 0, 0, 0, 0]); // op 0: disable all
    read_frame(&mut c);
    h.wait_subscriptions("client code removed", |s| last_sub(s) == Some((false, vec![0xB0C0])));
    drop(c);
    h.wait_status("client detached", |s| s["client"]["attached"] == 0);
    assert_eq!(h.router().subscriptions().len(), 3, "{:?}", h.router().subscriptions());
    assert_eq!(h.status().unwrap()["subscribed"], json!(["0xB0C1", "0xB0C2"]));
}

#[test]
fn sigterm_path_unsubscribes_deregisters_and_removes_the_status_file() {
    let mut h = Harness::start("term", json!({"default_codes": ["0xB0C1"]}));
    let mut c = h.connect();
    subscribe_b0c0(&mut c);
    h.shutdown().unwrap();
    let subs = h.router().subscriptions();
    assert_eq!(last_sub(&subs), Some((false, vec![0xB0C0, 0xB0C1])), "{subs:?}");
    assert_eq!(h.router().deregistrations(), vec![1]);
    assert!(!h.status.exists(), "status file removed");
    let mut b = [0u8; 1];
    let r = c.read(&mut b);
    assert!(matches!(r, Ok(0)) || r.is_err(), "client closed: {r:?}");
}

#[test]
fn exclusive_profile_is_refused_as_not_built() {
    let name = router_name("excl");
    let router = FakeRouter::start(&name);
    let dir = TempDir::new("excl");
    let status = dir.0.join("status.json");
    let mut opts = Opts::new(status.clone());
    opts.router_addr = Some(name);
    let p = profile(json!({"transport": "serial", "session": "exclusive"}));
    let e = daemon::run(p, opts).unwrap_err();
    assert!(e.to_string().contains("not built"), "{e}");
    assert!(router.registrations().is_empty());
    assert!(!status.exists());
}

#[test]
fn an_invalid_profile_is_refused_before_anything_happens() {
    let dir = TempDir::new("invalid");
    let status = dir.0.join("status.json");
    let p = profile(json!({"default_codes": ["B0C0"]}));
    let e = daemon::run(p, Opts::new(status.clone())).unwrap_err();
    assert!(e.to_string().contains("B0C0"), "{e}");
    assert!(!status.exists());
}

#[test]
fn garbage_from_the_client_is_answered_or_counted_and_changes_nothing() {
    let h = Harness::start("garbage", json!({}));
    let mut c = h.connect();
    // A frame whose CRC is wrong.
    let mut bad = Vec::new();
    hdlc_encode(&set_mask(0xB, &[0xC0]), &mut bad);
    bad[3] ^= 0x01;
    c.write_all(&bad).unwrap();
    // A command diagbuddy does not answer.
    send_req(&mut c, &[0x7D, 0x01, 0x02]);
    assert_eq!(read_frame(&mut c), vec![0x13, 0x7D, 0x01, 0x02]);
    let s = h.wait_status("garbage counted", |s| {
        s["client"]["bad_requests"] == 1 && s["client"]["refused_commands"] == json!({"0x7d": 1})
    });
    assert_eq!(s["subscribed"], json!([]));
    assert!(h.router().subscriptions().is_empty());
    assert!(h.daemon_alive());
    // Still serving.
    subscribe_b0c0(&mut c);
    assert_eq!(h.router().subscriptions(), vec![(true, vec![0xB0C0])]);
    // Refusals are kept across clients.
    drop(c);
    let mut c2 = h.connect();
    send_req(&mut c2, &[0x7D]);
    assert_eq!(read_frame(&mut c2), vec![0x13, 0x7D]);
    h.wait_status("refusals accumulate", |s| s["client"]["refused_commands"] == json!({"0x7d": 2}));
}

#[test]
fn the_status_file_has_the_documented_shape() {
    let before = std::time::SystemTime::now();
    let h = Harness::start("shape", json!({}));
    let s = h.status().unwrap();
    assert_eq!(s["pid"], std::process::id());
    let since = s["since"].as_str().unwrap();
    assert_eq!(since.len(), 20, "{since}");
    assert!(since.ends_with('Z') && &since[4..5] == "-" && &since[10..11] == "T", "{since}");
    // `since` is the daemon's start on the current clock: not before this test started (1 s
    // of rounding), and not after `updated`.
    let since_t = diagbuddy::daemon::parse_rfc3339(since).expect("since parses");
    let floor = before - std::time::Duration::from_secs(1);
    assert!(since_t >= floor, "since {since} is before the test started");
    let updated_t = diagbuddy::daemon::parse_rfc3339(s["updated"].as_str().unwrap()).unwrap();
    assert!(since_t <= updated_t, "since {since} is after updated");
    assert_rfc3339(&s["updated"]);
    assert_eq!(s["profile"], json!({"host": "test-host", "modem": "test-modem"}));
    assert_eq!(s["client_id"], 1);
    assert_eq!(s["dci_registrations"], 1);
    assert_eq!(s["dci_failures"], 0);
    assert_eq!(s["transport"], "diag-router");
    assert_eq!(
        s["client"],
        json!({"attached": 0, "max": 8, "refused_connections": 0, "sent": 0, "requests": 0,
               "dropped": 0, "bad_requests": 0, "refused_commands": {}, "over_cap": 0})
    );
    assert_eq!(s["clients"], json!([]));
    assert_eq!(s["subscribed"], json!([]));
    assert_eq!(s["records"], 0);
    assert_eq!(s["unparsed_bytes"], 0);
    assert_eq!(s["stock_consumers"], json!([]));
    assert!(s["listen"].as_str().unwrap().starts_with("tcp:127.0.0.1:"));
    let events = s["events"].as_array().unwrap();
    for e in events {
        assert_rfc3339(&e["time"]);
    }
    let msgs: Vec<&str> = events.iter().map(|e| e["msg"].as_str().unwrap()).collect();
    assert!(msgs[0].starts_with("serving tcp:127.0.0.1:"), "{msgs:?}");
    assert!(msgs.iter().any(|m| m.starts_with("DCI registered as client 1")), "{msgs:?}");
}

fn assert_rfc3339(v: &Value) {
    let t = v.as_str().unwrap_or_else(|| panic!("not a time: {v}"));
    assert_eq!(t.len(), 20, "{t}");
    assert!(t.ends_with('Z') && &t[4..5] == "-" && &t[10..11] == "T", "{t}");
}

fn event_msgs(s: &Value) -> Vec<String> {
    s["events"].as_array().unwrap().iter().map(|e| e["msg"].as_str().unwrap().to_string()).collect()
}

fn write_comm(root: &Path, pid: u32, comm: &str) {
    let d = root.join(pid.to_string());
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(d.join("comm"), format!("{comm}\n")).unwrap();
}

#[test]
fn stock_consumers_are_observed_present_restarted_and_absent() {
    let proc_dir = TempDir::new("proc");
    let root = proc_dir.0.clone();
    write_comm(&root, 17, "sh");
    write_comm(&root, 4242, "diag_read");
    std::fs::create_dir_all(root.join("self")).unwrap();
    let diag = json!({"stock_consumers": [
        {"name": "diag_read", "pidof": "diag_read"},
        {"name": "other_logger", "pidof": "other_logger"}
    ]});
    let r = root.clone();
    let h = Harness::start_with("consumers", diag, move |o| o.proc_root = r);
    let s = h.status().unwrap();
    assert_eq!(
        s["stock_consumers"],
        json!([
            {"name": "diag_read", "pid_at_start": 4242, "pid_now": 4242, "state": "present"},
            {"name": "other_logger", "pid_at_start": null, "pid_now": null, "state": "absent"}
        ])
    );
    std::fs::rename(root.join("4242"), root.join("5151")).unwrap();
    h.wait_status("restarted", |s| {
        s["stock_consumers"][0] == json!({"name": "diag_read", "pid_at_start": 4242, "pid_now": 5151, "state": "restarted"})
    });
    std::fs::remove_dir_all(root.join("5151")).unwrap();
    h.wait_status("absent", |s| {
        s["stock_consumers"][0] == json!({"name": "diag_read", "pid_at_start": 4242, "pid_now": null, "state": "absent"})
    });
    // Observation only: what was left in the fake /proc is untouched.
    assert_eq!(std::fs::read_to_string(root.join("17/comm")).unwrap(), "sh\n");
}

/// Every item of equipment id `equip` that a mask can carry: 4095 codes.
fn whole_equip(equip: u32) -> Vec<u8> {
    set_mask(equip, &(0..0xFFF).collect::<Vec<u32>>())
}

#[test]
fn a_router_that_stops_reading_is_lost_and_the_daemon_keeps_reporting_and_stopping() {
    let mut h = Harness::start("deaf", json!({}));
    let mut c = h.connect();
    subscribe_b0c0(&mut c);
    h.router().stop_reading();
    // Each request changes the subscription, so each is a DCI write the router never reads.
    // Replies are not read here; the daemon queues them.
    for i in 0..64 {
        let req = if i % 2 == 0 { whole_equip(0xB) } else { set_mask(0xB, &[0xC0]) };
        send_req(&mut c, &req);
    }
    h.wait_status_for(Duration::from_secs(20), "dci retrying after a write timeout", |s| {
        s["dci"] == "retrying"
    });
    // The supervisor is not wedged: the status file is rewritten, twice over.
    for _ in 0..2 {
        std::fs::remove_file(&h.status).unwrap();
        h.wait_status_for(Duration::from_secs(10), "status file rewritten", |s| s["dci"] == "retrying");
    }
    let t = Instant::now();
    h.shutdown().unwrap();
    assert!(t.elapsed() < Duration::from_secs(10), "stop took {:?}", t.elapsed());
}

#[test]
fn a_router_that_drops_every_session_at_once_is_retried_ever_more_slowly() {
    // Base 100 ms; every session is short, so no wait is ever reset to the base.
    let h = Harness::start_with("flap", json!({}), |o| o.retry_base = Duration::from_millis(100));
    let mut at = vec![Instant::now()];
    for n in 2..=6 {
        h.router().drop_client();
        wait_for(Duration::from_secs(10), "re-registration", || {
            (h.router().registrations().len() >= n).then_some(())
        });
        at.push(Instant::now());
    }
    let gaps: Vec<Duration> = at.windows(2).map(|w| w[1] - w[0]).collect();
    // 100, 200, 400, 800, 1600 ms plus up to a supervisor tick each.
    assert!(gaps[4] >= Duration::from_millis(1500), "{gaps:?}");
    assert!(gaps[4] >= gaps[0] * 4, "{gaps:?}");
    assert!(gaps.windows(2).all(|w| w[1] > w[0]), "{gaps:?}");
}

#[test]
fn a_kernel_char_profile_is_refused_as_not_built() {
    let name = router_name("kchar");
    let router = FakeRouter::start(&name);
    let dir = TempDir::new("kchar");
    let status = dir.0.join("status.json");
    let mut opts = Opts::new(status.clone());
    opts.router_addr = Some(name);
    let p = profile(json!({"transport": "kernel-char", "session": "dci"}));
    let e = daemon::run(p, opts).unwrap_err();
    assert!(e.to_string().contains("not built") && e.to_string().contains("kernel-char"), "{e}");
    assert!(router.registrations().is_empty());
    assert!(!status.exists());
}

fn codes_in(s: &Value) -> usize {
    s["subscribed"].as_array().map_or(0, Vec::len)
}

/// Send `req` and check the reply is the op 3 echo: a refused subscription is still answered.
fn set_and_read_echo(c: &mut TcpStream, req: &[u8]) {
    send_req(c, req);
    let resp = read_frame(c);
    assert_eq!(&resp[..8], &req[..8]);
    assert_eq!(&resp[8..12], &[0, 0, 0, 0], "status 0");
    assert_eq!(&resp[12..], &req[8..]);
}

#[test]
fn a_subscription_over_the_cap_is_refused_and_the_last_one_stays() {
    let h = Harness::start("cap", json!({}));
    let mut c = h.connect();
    // One whole equipment id, 4095 codes: what was measured on the unit.
    set_and_read_echo(&mut c, &whole_equip(0xB));
    let s = h.wait_status("4095 codes subscribed", |s| codes_in(s) == 4095);
    assert_eq!(s["client"]["over_cap"], 0);
    let writes = h.router().subscriptions().len();

    // A second whole equipment id would make 8190: refused, nothing written to the router.
    set_and_read_echo(&mut c, &whole_equip(0x1));
    let s = h.wait_status("over the cap counted", |s| s["client"]["over_cap"] == 1);
    assert_eq!(codes_in(&s), 4095);
    assert_eq!(s["subscribed"][0], "0xB000");
    assert_eq!(h.router().subscriptions().len(), writes);

    // Back under the cap (4095 + 1 = 4096): applied normally.
    set_and_read_echo(&mut c, &set_mask(0xB, &[0xC0]));
    let s = h.wait_status("back under the cap", |s| codes_in(s) == 4096);
    assert_eq!(s["subscribed"][0], "0x1000");
    assert_eq!(s["client"]["over_cap"], 1);
    assert_eq!(h.router().subscriptions().len(), writes + 2);
}

#[test]
fn the_status_file_keeps_the_last_20_log_lines() {
    let h = Harness::start("events", json!({}));
    for _ in 0..12 {
        let c = h.connect();
        drop(c);
        h.wait_status("client detached", |s| s["client"]["attached"] == 0);
    }
    let s = h.wait_status("12 detach lines", |s| {
        event_msgs(s).iter().filter(|m| is_detach(m)).count() >= 10
    });
    let msgs = event_msgs(&s);
    assert_eq!(msgs.len(), 20, "{msgs:?}");
    assert!(is_detach(msgs.last().unwrap()), "{msgs:?}");
    assert!(!msgs.iter().any(|m| m.starts_with("serving")), "the oldest lines are gone: {msgs:?}");
}

fn is_detach(m: &str) -> bool {
    m.starts_with("client ") && m.contains(" detached: ")
}

fn profile_with_stream(diag: Value, stream: Value) -> Profile {
    let mut p = profile(diag);
    let mut v = json!({"listen": "tcp:127.0.0.1:0"});
    diagbuddy::profile::merge(&mut v, &stream);
    p.stream = serde_json::from_value(v).unwrap();
    p
}

/// Read every frame until the stream has been quiet for `quiet`; return the log codes of the
/// log packets (0x10) in arrival order. Replies (0x73, 0x13) are not log packets.
fn drain_log_codes(c: &mut TcpStream, quiet: Duration) -> Vec<u16> {
    c.set_read_timeout(Some(quiet)).unwrap();
    let mut raw = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match c.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => raw.extend_from_slice(&buf[..n]),
            Err(_) => break,
        }
    }
    c.set_read_timeout(Some(WAIT)).unwrap();
    let mut codes = Vec::new();
    for chunk in raw.split_inclusive(|&b| b == 0x7E) {
        if chunk.len() < 2 {
            continue;
        }
        let pkt = hdlc_decode(chunk).expect("frame with a good CRC");
        if pkt[0] == 0x10 {
            codes.push(u16::from_le_bytes([pkt[6], pkt[7]]));
        }
    }
    codes
}

/// LOG_CONFIG op 3 for equipment id 0xB with `items`, reading the echo.
fn subscribe(c: &mut TcpStream, items: &[u32]) {
    set_and_read_echo(c, &set_mask(0xB, items));
}

const QUIET: Duration = Duration::from_millis(400);

#[test]
fn two_clients_with_disjoint_subsets_each_get_exactly_their_own_codes() {
    let h = Harness::start("disjoint", json!({}));
    let mut a = h.connect();
    let mut b = h.connect();
    subscribe(&mut a, &[0xC0]);
    subscribe(&mut b, &[0xC1]);
    h.wait_status("union subscribed", |s| s["subscribed"] == json!(["0xB0C0", "0xB0C1"]));
    h.router().push_logs(&[
        (0xB0C0, vec![1]),
        (0xB0C1, vec![2]),
        (0xB0C0, vec![3]),
        (0xB0C1, vec![4]),
        (0xB0C1, vec![5]),
    ]);
    assert_eq!(drain_log_codes(&mut a, QUIET), vec![0xB0C0, 0xB0C0]);
    assert_eq!(drain_log_codes(&mut b, QUIET), vec![0xB0C1, 0xB0C1, 0xB0C1]);
    let s = h.wait_status("per-client counters", |s| {
        s["clients"].as_array().is_some_and(|c| c.len() == 2 && c[1]["sent"] == 4)
    });
    assert_eq!(s["client"]["attached"], 2);
    assert_eq!(s["clients"][0]["codes"], json!(["0xB0C0"]));
    assert_eq!(s["clients"][1]["codes"], json!(["0xB0C1"]));
    assert_eq!(s["clients"][0]["requests"], 1);
    assert_eq!(s["clients"][0]["sent"], 3, "one reply and two records");
}

#[test]
fn three_clients_with_overlapping_subsets_get_exactly_theirs_and_leaving_narrows_the_union() {
    let h = Harness::start("overlap", json!({}));
    let mut a = h.connect();
    let mut b = h.connect();
    let mut c = h.connect();
    subscribe(&mut a, &[0xC0, 0xC1]);
    subscribe(&mut b, &[0xC1, 0xC2]);
    subscribe(&mut c, &[0xC2]);
    h.wait_status("union", |s| s["subscribed"] == json!(["0xB0C0", "0xB0C1", "0xB0C2"]));
    h.router().push_logs(&[(0xB0C0, vec![1]), (0xB0C1, vec![2]), (0xB0C2, vec![3])]);
    assert_eq!(drain_log_codes(&mut a, QUIET), vec![0xB0C0, 0xB0C1]);
    assert_eq!(drain_log_codes(&mut b, QUIET), vec![0xB0C1, 0xB0C2]);
    assert_eq!(drain_log_codes(&mut c, QUIET), vec![0xB0C2]);

    // b leaves: 0xB0C1 is still a's and 0xB0C2 still c's, so nothing is unsubscribed.
    let writes = h.router().subscriptions().len();
    drop(b);
    h.wait_status("b detached", |s| s["client"]["attached"] == 2);
    assert_eq!(h.router().subscriptions().len(), writes, "{:?}", h.router().subscriptions());
    assert_eq!(h.status().unwrap()["subscribed"], json!(["0xB0C0", "0xB0C1", "0xB0C2"]));

    // a leaves: 0xB0C0 and 0xB0C1 were only a's now; 0xB0C2 stays for c.
    drop(a);
    h.wait_subscriptions("a's codes leave", |s| last_sub(s) == Some((false, vec![0xB0C0, 0xB0C1])));
    h.wait_status("narrowed", |s| s["subscribed"] == json!(["0xB0C2"]));
    h.router().push_logs(&[(0xB0C2, vec![4])]);
    assert_eq!(drain_log_codes(&mut c, QUIET), vec![0xB0C2]);
}

#[test]
fn a_client_narrowing_its_mask_keeps_codes_another_client_still_wants() {
    let h = Harness::start("narrow", json!({}));
    let mut a = h.connect();
    let mut b = h.connect();
    subscribe(&mut a, &[0xC0, 0xC1]);
    subscribe(&mut b, &[0xC0]);
    let writes = h.router().subscriptions().len();
    // a drops 0xB0C0: b still wants it, so the subscription does not change.
    subscribe(&mut a, &[0xC1]);
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(h.router().subscriptions().len(), writes);
    h.router().push_logs(&[(0xB0C0, vec![1]), (0xB0C1, vec![2])]);
    assert_eq!(drain_log_codes(&mut a, QUIET), vec![0xB0C1], "a no longer gets 0xB0C0");
    assert_eq!(drain_log_codes(&mut b, QUIET), vec![0xB0C0]);
    // op 0 from b disables only b's codes.
    send_req(&mut b, &[0x73, 0, 0, 0, 0, 0, 0, 0]);
    read_frame(&mut b);
    h.wait_subscriptions("0xB0C0 leaves", |s| last_sub(s) == Some((false, vec![0xB0C0])));
    h.router().push_logs(&[(0xB0C1, vec![3])]);
    assert_eq!(drain_log_codes(&mut a, QUIET), vec![0xB0C1]);
}

#[test]
fn each_client_reads_back_its_own_mask() {
    let h = Harness::start("ownmask", json!({}));
    let mut a = h.connect();
    let mut b = h.connect();
    subscribe(&mut a, &[0xC0]);
    send_req(&mut b, &get_mask(0xB));
    let resp = read_frame(&mut b);
    let mut want = vec![0x73, 0, 0, 0, 4, 0, 0, 0, 0, 0, 0, 0, 0x0B, 0, 0, 0];
    want.extend(vec![0u8; 0x1000 / 8]);
    assert_eq!(resp, want, "a's set-mask is not b's");
    send_req(&mut a, &get_mask(0xB));
    let resp = read_frame(&mut a);
    assert_eq!(resp[16 + 0xC0 / 8], 1, "a sees its own bit");
}

#[test]
fn a_stalled_client_drops_its_own_frames_and_never_holds_up_another() {
    let h = Harness::start("stall", json!({}));
    let mut stalled = h.connect();
    let mut live = h.connect();
    subscribe(&mut stalled, &[0xC0]);
    subscribe(&mut live, &[0xC0]);
    // Rounds of 100 records (one DCI message, well under the seqpacket limit); the reading
    // client takes each round before the next, so it never falls behind. The stalled peer never
    // reads: 30 MB is far more than its queue and its socket buffers hold.
    let big = vec![0x55u8; 1000];
    const ROUND: usize = 100;
    const ROUNDS: usize = 300;
    // A hold-up is one round that waits on the stalled peer: a write that blocks until the
    // 5 s write timeout detaches it. So bound the slowest round, not the total: the total is
    // ~100 ms a round of harness overhead and sat within ~1 s of a 30 s bound on a loaded
    // host, while no healthy round comes near 2 s.
    const SLOWEST_ROUND: Duration = Duration::from_secs(2);
    let t0 = Instant::now();
    let mut slowest = (Duration::ZERO, 0);
    for r in 0..ROUNDS {
        let tr = Instant::now();
        h.router().push_logs(&vec![(0xB0C0, big.clone()); ROUND]);
        for k in 0..ROUND {
            assert_eq!(read_frame(&mut live)[0], 0x10, "round {r} record {k}");
        }
        slowest = slowest.max((tr.elapsed(), r));
    }
    assert!(
        slowest.0 < SLOWEST_ROUND,
        "round {} took {:?} (all {ROUNDS} rounds: {:?})",
        slowest.1,
        slowest.0,
        t0.elapsed()
    );
    // The stalled client (id 1) may still be attached with its drops counted, or its writer
    // may have timed out and detached it; either way the drops land in the lifetime total,
    // and none of them are the reading client's (id 2).
    let s = h.wait_status("stalled client's drops counted", |s| {
        s["client"]["dropped"].as_u64() > Some(0)
    });
    let clients = s["clients"].as_array().unwrap();
    let live_row = clients.iter().find(|c| c["id"] == 2).expect("reading client still attached");
    assert_eq!(live_row["dropped"], 0, "{}", s["clients"]);
    assert!(clients.iter().all(|c| c["id"] == 1 || c["id"] == 2), "{}", s["clients"]);
    drop(stalled);
}

#[test]
fn a_client_over_the_limit_is_refused_logged_and_counted() {
    let name = "limit";
    let h = Harness::start_with_profile(name, profile_with_stream(json!({}), json!({"max_clients": 2})));
    let _a = h.connect();
    let _b = h.connect();
    let mut c = TcpStream::connect(h.addr()).unwrap();
    c.set_read_timeout(Some(WAIT)).unwrap();
    let mut buf = [0u8; 1];
    let r = c.read(&mut buf);
    assert!(matches!(r, Ok(0)) || r.is_err(), "third client closed: {r:?}");
    let s = h.wait_status("refusal counted", |s| s["client"]["refused_connections"] == 1);
    assert_eq!(s["client"]["attached"], 2);
    assert_eq!(s["client"]["max"], 2);
    assert!(
        event_msgs(&s).iter().any(|m| m.starts_with("refused 1 stream connection(s): 2 clients")),
        "{:?}",
        event_msgs(&s)
    );
    // One leaves; the next is admitted.
    drop(_a);
    h.wait_status("one left", |s| s["client"]["attached"] == 1);
    let _d = h.connect();
}

#[test]
fn the_cap_applies_to_the_union_and_refusing_one_client_leaves_the_others_alone() {
    let h = Harness::start("ucap", json!({}));
    let mut a = h.connect();
    let mut b = h.connect();
    set_and_read_echo(&mut a, &whole_equip(0xB));
    h.wait_status("4095 subscribed", |s| codes_in(s) == 4095);
    // b asks for two codes of another equipment id: the union would be 4097.
    set_and_read_echo(&mut b, &set_mask(0x1, &[1, 2]));
    let s = h.wait_status("b over the cap", |s| s["client"]["over_cap"] == 1);
    assert_eq!(codes_in(&s), 4095);
    assert_eq!(s["clients"][1]["over_cap"], 1);
    assert_eq!(s["clients"][1]["codes"], json!([]));
    assert_eq!(s["clients"][0]["over_cap"], 0);
    // a still gets its records; b gets none (it was granted nothing).
    h.router().push_logs(&[(0xB0C0, vec![1])]);
    assert_eq!(drain_log_codes(&mut a, QUIET), vec![0xB0C0]);
    assert_eq!(drain_log_codes(&mut b, QUIET), Vec::<u16>::new());
    // One code fits (4096): b is granted it.
    set_and_read_echo(&mut b, &set_mask(0x1, &[1]));
    h.wait_status("b granted", |s| codes_in(s) == 4096 && s["clients"][1]["codes"] == json!(["0x1001"]));
}

#[test]
fn default_codes_are_subscribed_but_reach_only_clients_that_ask_for_them() {
    let h = Harness::start("defonly", json!({"default_codes": ["0xB0C1"]}));
    let mut a = h.connect();
    let mut b = h.connect();
    subscribe(&mut a, &[0xC0]);
    subscribe(&mut b, &[0xC1]);
    h.router().push_logs(&[(0xB0C0, vec![1]), (0xB0C1, vec![2])]);
    assert_eq!(drain_log_codes(&mut a, QUIET), vec![0xB0C0], "a never asked for the default");
    assert_eq!(drain_log_codes(&mut b, QUIET), vec![0xB0C1]);
}
