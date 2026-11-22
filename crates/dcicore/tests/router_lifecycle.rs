// SPDX-License-Identifier: Apache-2.0
#[path = "common/fake_router.rs"]
mod fake_router;

use dcicore::dci::{log_mask_msg, parse_data};
use dcicore::transport::Transport;
use fake_router::FakeRouter;
use std::io::Read;

fn read_codes(t: &Transport) -> Vec<u16> {
    let mut buf = vec![0u8; 65536];
    let n = t.reader().unwrap().read(&mut buf).unwrap();
    parse_data(&buf[..n]).unwrap().records.iter().map(|r| r.code).collect()
}

#[test]
fn drop_client_closes_the_connection_and_the_router_accepts_the_next_one() {
    let r = FakeRouter::start(&fake_router::unique("dcicore-test-reconnect"));

    let mut a = Transport::open_router_at(&fake_router::unique("dcicore-test-reconnect")).unwrap();
    a.handshake().unwrap();
    a.send(&log_mask_msg(1, true, &[0xB063])).unwrap();
    r.drop_client();
    // The first client sees end of stream.
    let mut buf = [0u8; 64];
    assert_eq!(a.reader().unwrap().read(&mut buf).unwrap(), 0);

    let mut b = Transport::open_router_at(&fake_router::unique("dcicore-test-reconnect")).unwrap();
    b.handshake().unwrap();
    b.send(&log_mask_msg(1, false, &[0xB064, 0xB065])).unwrap();
    r.push_logs(&[(0xB064, vec![]), (0xB065, vec![9])]);
    assert_eq!(read_codes(&b), vec![0xB064, 0xB065]);

    // Both connections' traffic accumulated.
    assert_eq!(r.registrations().len(), 2);
    assert_eq!(r.subscriptions(), vec![(true, vec![0xB063]), (false, vec![0xB064, 0xB065])]);
}

#[test]
fn dropping_the_router_frees_its_name_for_a_new_one() {
    drop(FakeRouter::start(&fake_router::unique("dcicore-test-rebind")));
    let r = FakeRouter::start(&fake_router::unique("dcicore-test-rebind"));
    let mut t = Transport::open_router_at(&fake_router::unique("dcicore-test-rebind")).unwrap();
    assert_eq!(t.handshake().unwrap().client_id, 1);
    assert_eq!(r.registrations().len(), 1);
}

#[test]
fn nothing_listens_on_an_unused_name() {
    assert!(Transport::open_router_at(&fake_router::unique("dcicore-test-nobody-home")).is_err());
}

#[test]
fn a_write_to_a_router_that_stopped_reading_fails_instead_of_blocking() {
    let r = FakeRouter::start(&fake_router::unique("dcicore-test-deaf"));
    let mut t = Transport::open_router_at(&fake_router::unique("dcicore-test-deaf")).unwrap();
    t.handshake().unwrap();
    r.stop_reading();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        // A whole equipment id per message; the socket fills within a few dozen.
        let codes: Vec<u16> = (0xB001..=0xBFFF).collect();
        let err = (0..1000).find_map(|i| t.send(&log_mask_msg(1, i % 2 == 0, &codes)).err());
        let _ = tx.send(err);
    });
    let err = rx
        .recv_timeout(std::time::Duration::from_secs(20))
        .expect("a send to a router that stopped reading is still blocked after 20 s");
    let e = err.expect("1000 sends to a router that stopped reading all succeeded");
    assert!(
        matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut),
        "{e:?}"
    );
}
