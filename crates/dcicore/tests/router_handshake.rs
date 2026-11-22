// SPDX-License-Identifier: Apache-2.0
#[path = "common/fake_router.rs"]
mod fake_router;

use dcicore::dci::{log_mask_msg, parse_data};
use dcicore::transport::Transport;
use fake_router::FakeRouter;
use std::io::Read;
use std::os::raw::{c_int, c_void};
use std::time::{Duration, Instant};

#[test]
fn registers_with_its_pid_and_reads_a_log_record() {
    let r = FakeRouter::start(&fake_router::unique("dcicore-test-handshake"));
    let mut t = Transport::open_router_at(&fake_router::unique("dcicore-test-handshake")).unwrap();
    let s = t.handshake().unwrap();
    assert_eq!(s.client_id, 1);
    assert_eq!(s.support, 0x0003);
    let reg = r.registrations()[0];
    assert_eq!(&reg[0..4], &std::process::id().to_le_bytes());
    assert_eq!(&reg[4..8], &(-1i32).to_le_bytes());
    t.send(&log_mask_msg(1, true, &[0xB0C0])).unwrap();
    r.push_logs(&[(0xB0C0, vec![1, 2, 3])]);
    let mut buf = vec![0u8; 65536];
    let n = t.reader().unwrap().read(&mut buf).unwrap();
    let p = parse_data(&buf[..n]).unwrap();
    assert_eq!(p.records.len(), 1);
    assert_eq!(p.records[0].code, 0xB0C0);
    assert_eq!(&p.records[0].log[12..], &[1, 2, 3]);
    assert_eq!(r.subscriptions(), vec![(true, vec![0xB0C0])]);
}

#[test]
fn a_handshake_that_fails_after_registering_deregisters_its_client_id() {
    // The router gave a client id, then never answered the support query. The id is live
    // in the router's table; closing without a deregister would leave it there.
    let r = FakeRouter::start(&fake_router::unique("dcicore-test-handshake-late-fail"));
    r.ignore_support();
    let mut t = Transport::open_router_at(&fake_router::unique("dcicore-test-handshake-late-fail")).unwrap();
    assert!(t.handshake().is_err());
    assert_eq!(r.registrations().len(), 1);
    assert_eq!(r.deregistrations(), vec![1]);
}

#[test]
fn a_router_that_chatters_but_never_answers_is_given_up_on_by_the_deadline() {
    // Each read returns (an untagged message every 200 ms), so a per-read timeout never fires
    // and only the 12-read cap ended the wait: up to 12 x 3 s per awaited reply, on the
    // daemon's supervisor thread, which also writes the status file and answers SIGTERM.
    let r = FakeRouter::start(&fake_router::unique("dcicore-test-handshake-chatter"));
    r.chatter(Duration::from_millis(200));
    let mut t = Transport::open_router_at(&fake_router::unique("dcicore-test-handshake-chatter")).unwrap();
    let t0 = Instant::now();
    let e = t.handshake_within(Duration::from_millis(1000)).unwrap_err();
    let took = t0.elapsed();
    assert!(took < Duration::from_millis(1800), "handshake took {took:?}");
    assert!(e.to_string().contains("0x10000"), "{e}");
}

#[test]
fn a_router_that_never_reads_is_given_up_on_by_the_deadline() {
    let r = FakeRouter::start(&fake_router::unique("dcicore-test-handshake-deaf"));
    r.stop_reading();
    let mut t = Transport::open_router_at(&fake_router::unique("dcicore-test-handshake-deaf")).unwrap();
    let t0 = Instant::now();
    assert!(t.handshake_within(Duration::from_millis(600)).is_err());
    assert!(t0.elapsed() < Duration::from_millis(1400), "handshake took {:?}", t0.elapsed());
}

#[test]
fn the_default_handshake_deadline_is_bounded() {
    assert!(dcicore::transport::ROUTER_HANDSHAKE_TIMEOUT <= Duration::from_secs(10));
}

extern "C" {
    fn socket(domain: c_int, ty: c_int, proto: c_int) -> c_int;
    fn bind(fd: c_int, addr: *const c_void, len: u32) -> c_int;
    fn listen(fd: c_int, backlog: c_int) -> c_int;
    fn close(fd: c_int) -> c_int;
}

#[test]
fn connect_to_a_router_whose_backlog_is_full_times_out() {
    // A listener that never accepts, backlog 0: the first connect is queued, the next one
    // would sleep in connect() until the router accepts. connect had no timeout.
    let name_owned = fake_router::unique("dcicore-test-connect-full-backlog");
    let name = &name_owned[..];
    let addr = dcicore::sys::abstract_sockaddr(name).unwrap();
    let fd = unsafe { socket(1, 5 | 0o2000000, 0) };
    assert!(fd >= 0);
    assert_eq!(unsafe { bind(fd, addr.as_ptr() as *const c_void, addr.len() as u32) }, 0);
    assert_eq!(unsafe { listen(fd, 0) }, 0);
    let first = Transport::open_router_at(name);
    assert!(first.is_ok(), "first connect should be queued");
    let t0 = Instant::now();
    let second = dcicore::sys::connect_router_within(name, Duration::from_millis(500));
    let took = t0.elapsed();
    unsafe { close(fd) };
    assert!(second.is_err(), "second connect should time out");
    assert!(took < Duration::from_millis(1500), "connect took {took:?}");
}
