// SPDX-License-Identifier: Apache-2.0
//! diagbuddy command-line dispatch. Logic lives in the library modules.

use diagbuddy::daemon::{self, Opts};
use diagbuddy::profile;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::Ordering;
use std::time::{Duration, SystemTime};

const USAGE: &str = "usage:
  diagbuddy validate <file>...
  diagbuddy run [--profiles-dir D]... [--host H] [--modem M] [--set key=json]... [--status-file P]
  diagbuddy status [--status-file P]";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let rest = args.get(1..).unwrap_or(&[]);
    match args.first().map(String::as_str) {
        Some("validate") if !rest.is_empty() => validate(rest),
        Some("run") => run(rest),
        Some("status") => status(rest),
        _ => {
            eprintln!("{USAGE}");
            ExitCode::from(2)
        }
    }
}

fn validate(files: &[String]) -> ExitCode {
    let mut ok = true;
    for f in files {
        match profile::validate_file(Path::new(f)) {
            Ok(()) => println!("{f}: ok"),
            Err(e) => {
                ok = false;
                println!("{f}: error: {e}");
            }
        }
    }
    ExitCode::from(if ok { 0 } else { 1 })
}

struct RunArgs {
    dirs: Vec<PathBuf>,
    host: String,
    modem: String,
    overrides: Value,
    status_file: PathBuf,
}

/// `--set a.b=<json>` as the merge patch `{"a": {"b": <json>}}`.
fn set_override(spec: &str) -> Result<Value, String> {
    let (key, value) = spec.split_once('=').ok_or_else(|| format!("--set {spec}: expected key=json"))?;
    if key.is_empty() || key.split('.').any(str::is_empty) {
        return Err(format!("--set {spec}: bad key \"{key}\""));
    }
    let mut v: Value = serde_json::from_str(value)
        .map_err(|e| format!("--set {key}: value is not JSON ({e})"))?;
    for part in key.rsplit('.') {
        let mut m = serde_json::Map::new();
        m.insert(part.to_string(), v);
        v = Value::Object(m);
    }
    Ok(v)
}

fn parse_run(args: &[String]) -> Result<RunArgs, String> {
    let mut r = RunArgs {
        dirs: Vec::new(),
        host: "auto".into(),
        modem: "auto".into(),
        overrides: Value::Object(Default::default()),
        status_file: PathBuf::from(daemon::DEFAULT_STATUS_FILE),
    };
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        let mut value = || it.next().cloned().ok_or_else(|| format!("{flag} needs a value"));
        match flag.as_str() {
            "--profiles-dir" => r.dirs.push(PathBuf::from(value()?)),
            "--host" => r.host = value()?,
            "--modem" => r.modem = value()?,
            "--set" => profile::merge(&mut r.overrides, &set_override(&value()?)?),
            "--status-file" => r.status_file = PathBuf::from(value()?),
            other => return Err(format!("unknown argument \"{other}\"")),
        }
    }
    if r.dirs.is_empty() {
        r.dirs = profile::DEFAULT_DIRS.iter().map(PathBuf::from).collect();
    }
    Ok(r)
}

fn status_file_arg(args: &[String]) -> Result<PathBuf, String> {
    match args {
        [] => Ok(PathBuf::from(daemon::DEFAULT_STATUS_FILE)),
        [f, p] if f == "--status-file" => Ok(PathBuf::from(p)),
        _ => Err(format!("unexpected arguments {args:?}")),
    }
}

fn usage_error(e: &str) -> ExitCode {
    eprintln!("diagbuddy: {e}\n{USAGE}");
    ExitCode::from(2)
}

fn run(args: &[String]) -> ExitCode {
    let a = match parse_run(args) {
        Ok(a) => a,
        Err(e) => return usage_error(&e),
    };
    let (p, host, modem) = match profile::resolve_named(&a.dirs, &a.host, &a.modem, &a.overrides) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("diagbuddy: {e}");
            return ExitCode::from(1);
        }
    };
    let mut opts = Opts::new(a.status_file);
    opts.host = host;
    opts.modem = modem;
    // SIGTERM / SIGINT set dcicore's stop flag; hand it on to the daemon's stop handle, and
    // SIGUSR2's re-registration flag to its reregister handle. SIGPIPE is
    // already ignored: the Rust runtime sets it to SIG_IGN before `main` (a client that goes
    // away costs a write error, not the process).
    dcicore::sys::install_signal_handlers();
    let stop = opts.stop.clone();
    let reregister = opts.reregister.clone();
    std::thread::spawn(move || loop {
        if dcicore::sys::STOP.load(Ordering::Relaxed) {
            stop.request();
            return;
        }
        if dcicore::sys::REREGISTER.swap(false, Ordering::Relaxed) {
            reregister.request();
        }
        std::thread::sleep(Duration::from_millis(100));
    });
    // Returning from main ends the process even if a thread is still parked in a read.
    match daemon::run(p, opts) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("diagbuddy: {e}");
            ExitCode::from(1)
        }
    }
}

/// The pid in a status file, if that process is a running diagbuddy.
fn live_pid(text: &str) -> Option<u64> {
    let pid = serde_json::from_str::<Value>(text).ok()?["pid"].as_u64()?;
    let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
    (comm.trim_end() == "diagbuddy").then_some(pid)
}

/// A live daemon rewrites its status at least every `STATUS_INTERVAL`; one older than this
/// is from a daemon that is wedged.
const STALE_AFTER: Duration = Duration::from_secs(3 * daemon::STATUS_INTERVAL.as_secs());

/// `Err(why)` when the status's `updated` stamp is missing, unreadable or older than
/// [`STALE_AFTER`] at `now`. A stamp in the future (the clock stepped back) is not stale.
fn staleness(text: &str, now: SystemTime) -> Result<(), String> {
    let updated = serde_json::from_str::<Value>(text).ok().and_then(|v| v["updated"].as_str().map(String::from));
    let Some(at) = updated.as_deref().and_then(daemon::parse_rfc3339) else {
        return Err("status has no valid \"updated\" stamp; treating it as stale".to_string());
    };
    match now.duration_since(at) {
        Ok(age) if age > STALE_AFTER => Err(format!(
            "status is stale: last updated {} ({} s ago, more than {} s); the daemon may be wedged",
            updated.unwrap_or_default(),
            age.as_secs(),
            STALE_AFTER.as_secs()
        )),
        _ => Ok(()),
    }
}

/// Exit 0 with the status printed; 1 when no daemon runs; 3 when one runs but its status is
/// stale (printed all the same, with the reason on stderr).
fn status(args: &[String]) -> ExitCode {
    let path = match status_file_arg(args) {
        Ok(p) => p,
        Err(e) => return usage_error(&e),
    };
    match std::fs::read_to_string(&path) {
        Ok(text) if live_pid(&text).is_some() => {
            print!("{text}");
            match staleness(&text, SystemTime::now()) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("diagbuddy: {e}");
                    ExitCode::from(3)
                }
            }
        }
        _ => {
            eprintln!("diagbuddy: not running");
            ExitCode::from(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::UNIX_EPOCH;

    fn strings(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn set_nests_dotted_keys_and_merges_repeats() {
        let r = parse_run(&strings(&[
            "--set", "diag.session=\"dci\"",
            "--set", "diag.default_codes=[\"0xB0C0\"]",
            "--set", "stream={\"listen\":\"tcp:127.0.0.1:0\"}",
        ]))
        .unwrap();
        assert_eq!(
            r.overrides,
            json!({"diag": {"session": "dci", "default_codes": ["0xB0C0"]},
                   "stream": {"listen": "tcp:127.0.0.1:0"}})
        );
    }

    #[test]
    fn set_with_bad_json_names_the_key() {
        let e = parse_run(&strings(&["--set", "diag.session=dci"])).err().unwrap();
        assert!(e.contains("diag.session") && e.contains("not JSON"), "{e}");
        assert!(parse_run(&strings(&["--set", "nokey"])).is_err());
        assert!(parse_run(&strings(&["--set", "a..b=1"])).is_err());
    }

    #[test]
    fn a_fresh_status_is_not_stale() {
        let now = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let text = r#"{"pid": 1, "updated": "2023-11-14T22:13:10Z"}"#; // 10 s before `now`
        assert_eq!(staleness(text, now), Ok(()));
        // A clock stepped back after the write is not staleness.
        let text = r#"{"pid": 1, "updated": "2023-11-14T22:14:00Z"}"#;
        assert_eq!(staleness(text, now), Ok(()));
    }

    #[test]
    fn a_status_older_than_three_intervals_is_stale() {
        let now = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let text = r#"{"pid": 1, "updated": "2023-11-14T22:13:04Z"}"#; // 16 s before `now`
        let e = staleness(text, now).unwrap_err();
        assert!(e.contains("2023-11-14T22:13:04Z") && e.contains("16 s"), "{e}");
    }

    #[test]
    fn a_status_without_updated_is_stale() {
        let now = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        for text in [r#"{"pid": 1}"#, r#"{"pid": 1, "updated": "yesterday"}"#] {
            assert!(staleness(text, now).unwrap_err().contains("updated"), "{text}");
        }
    }

    #[test]
    fn run_defaults_and_flags() {
        let r = parse_run(&[]).unwrap();
        assert_eq!(r.dirs, profile::DEFAULT_DIRS.iter().map(PathBuf::from).collect::<Vec<_>>());
        assert_eq!((r.host.as_str(), r.modem.as_str()), ("auto", "auto"));
        assert_eq!(r.status_file, PathBuf::from(daemon::DEFAULT_STATUS_FILE));
        let r = parse_run(&strings(&[
            "--profiles-dir", "/a", "--profiles-dir", "/b", "--host", "h", "--modem", "m",
            "--status-file", "/s.json",
        ]))
        .unwrap();
        assert_eq!(r.dirs, vec![PathBuf::from("/a"), PathBuf::from("/b")]);
        assert_eq!((r.host.as_str(), r.modem.as_str()), ("h", "m"));
        assert_eq!(r.status_file, PathBuf::from("/s.json"));
        assert!(parse_run(&strings(&["--host"])).is_err());
        assert!(parse_run(&strings(&["--bogus"])).is_err());
    }
}
