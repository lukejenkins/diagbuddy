// SPDX-License-Identifier: Apache-2.0
//! The `diagbuddy` binary: `run` until SIGTERM, `status`, and the process-level details that
//! only show in a real process (signal dispositions, exit codes).
//!
//! No diag-router runs on a build host, so the daemon here sits in `retrying`; the DCI side is
//! covered against `FakeRouter` in `tests/daemon.rs`.

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_diagbuddy");

struct TempDir(PathBuf);
impl TempDir {
    fn new(tag: &str) -> TempDir {
        let p = std::env::temp_dir().join(format!("dbc-{tag}-{}", std::process::id()));
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

/// Kill the child if a test fails before it exits.
struct Daemon(Child);
impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn write_profiles(dir: &Path) {
    std::fs::create_dir_all(dir.join("host")).unwrap();
    std::fs::create_dir_all(dir.join("modem")).unwrap();
    std::fs::write(
        dir.join("host/test-host.json"),
        r#"{"modem": "test-modem", "spec_version": 1, "storage": {"recordings_path": "/nonexistent"}}"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("modem/test-modem.json"),
        r#"{"spec_version": 1, "diag": {"transport": "diag-router", "session": "dci"}}"#,
    )
    .unwrap();
}

fn status_cmd(file: &Path) -> Output {
    Command::new(BIN).arg("status").arg("--status-file").arg(file).output().unwrap()
}

fn wait_for(what: &str, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Signals the process ignores, from `/proc/<pid>/status`.
fn ignored_signals(pid: u32) -> u64 {
    let s = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
    let line = s.lines().find(|l| l.starts_with("SigIgn:")).unwrap();
    u64::from_str_radix(line["SigIgn:".len()..].trim(), 16).unwrap()
}

#[test]
fn run_until_sigterm_with_status_reporting_and_sigpipe_ignored() {
    let dir = TempDir::new("run");
    let profiles = dir.0.join("profiles");
    write_profiles(&profiles);
    let status = dir.0.join("status.json");
    let child = Command::new(BIN)
        .args(["run", "--profiles-dir"])
        .arg(&profiles)
        .args(["--host", "test-host", "--modem", "auto"])
        .args(["--set", "stream.listen=\"tcp:127.0.0.1:0\""])
        .arg("--status-file")
        .arg(&status)
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut d = Daemon(child);
    let pid = d.0.id();
    wait_for("status file", || status.exists());

    const SIGPIPE: u32 = 13;
    const SIGHUP: u32 = 1;
    let ign = ignored_signals(pid);
    assert_ne!(ign & (1 << (SIGPIPE - 1)), 0, "SIGPIPE must be ignored (SigIgn {ign:#x})");
    assert_ne!(ign & (1 << (SIGHUP - 1)), 0, "SIGHUP must be ignored (SigIgn {ign:#x})");

    let out = status_cmd(&status);
    assert!(out.status.success(), "{out:?}");
    let s: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(s["pid"], pid);
    assert_eq!(s["profile"]["host"], "test-host");
    assert_eq!(s["profile"]["modem"], "test-modem");

    let killed = Command::new("kill").args(["-TERM", &pid.to_string()]).status().unwrap();
    assert!(killed.success());
    let deadline = Instant::now() + Duration::from_secs(5);
    let code = loop {
        if let Some(st) = d.0.try_wait().unwrap() {
            break st;
        }
        assert!(Instant::now() < deadline, "daemon did not exit on SIGTERM");
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(code.code(), Some(0), "{code:?}");
    assert!(!status.exists(), "status file removed on exit");

    let out = status_cmd(&status);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(String::from_utf8_lossy(&out.stderr).trim(), "diagbuddy: not running");
}

#[test]
fn status_of_a_dead_pid_is_not_running() {
    let dir = TempDir::new("dead");
    let status = dir.0.join("status.json");
    // A pid that is not a diagbuddy: this test process.
    std::fs::write(&status, format!("{{\"pid\": {}}}", std::process::id())).unwrap();
    let out = status_cmd(&status);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(String::from_utf8_lossy(&out.stderr).trim(), "diagbuddy: not running");
}

#[test]
fn status_of_a_live_daemon_that_stopped_writing_is_stale() {
    let dir = TempDir::new("stale");
    let profiles = dir.0.join("profiles");
    write_profiles(&profiles);
    let status = dir.0.join("status.json");
    let child = Command::new(BIN)
        .args(["run", "--profiles-dir"])
        .arg(&profiles)
        .args(["--host", "test-host", "--modem", "auto"])
        .args(["--set", "stream.listen=\"tcp:127.0.0.1:0\""])
        .arg("--status-file")
        .arg(&status)
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let d = Daemon(child);
    wait_for("status file", || status.exists());
    // Stop the daemon so it cannot rewrite the file, then age the file's stamp.
    let pid = d.0.id().to_string();
    assert!(Command::new("kill").args(["-STOP", &pid]).status().unwrap().success());
    let mut s: Value = serde_json::from_slice(&std::fs::read(&status).unwrap()).unwrap();
    s["updated"] = "2000-01-01T00:00:00Z".into();
    std::fs::write(&status, serde_json::to_vec(&s).unwrap()).unwrap();
    let out = status_cmd(&status);
    let _ = Command::new("kill").args(["-CONT", &pid]).status();
    assert_eq!(out.status.code(), Some(3), "{out:?}");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("stale") && err.contains("2000-01-01T00:00:00Z"), "{err}");
    // The file is still printed.
    let printed: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(printed["pid"], d.0.id());
    drop(d);
}

#[test]
fn run_with_an_unresolvable_profile_exits_1() {
    let dir = TempDir::new("noprof");
    let out = Command::new(BIN)
        .args(["run", "--profiles-dir"])
        .arg(&dir.0)
        .args(["--host", "nope", "--status-file"])
        .arg(dir.0.join("s.json"))
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("nope"), "{out:?}");
}

#[test]
fn bad_set_value_is_a_usage_error_naming_the_key() {
    let out = Command::new(BIN).args(["run", "--set", "diag.session=dci"]).output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("diag.session"), "{out:?}");
}
