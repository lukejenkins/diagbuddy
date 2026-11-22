# diagbuddy

**License: Apache-2.0.** All of the code in this repository is licensed under the
[Apache License 2.0](LICENSE).

**A DIAG log streamer for OpenWrt cellular routers.** `diagbuddy` holds its own DCI
subscription on the device's DIAG stack, beside the vendor's stock DIAG consumers, and
serves the log records it receives over a local TCP stream. Any number of readers can
attach, each with its own log mask; [rayhunter](https://github.com/EFForg/rayhunter)
reads it with `device = "external"`.

It never stops, restarts or reconfigures a stock DIAG consumer or the diag-router, and it
never writes the main log, F3 or event masks: it touches only its own DCI subscription.
The device binary is a static aarch64 musl build that needs nothing beyond the kernel.
This repository is the daemon and its library only; it does not include OpenWrt
packaging.

## Tested hardware

| Host | Modem | Transport | State |
|---|---|---|---|
| Inseego FX4200 (OpenWrt 23.05) | SDX7x, firmware HLA-2.27.1 | userspace diag-router, DCI | LTE log codes stream; this firmware emits no NR RRC/NAS log codes |

What was measured on each host is in `profiles/` (`evidence`). A new host or modem is a
pair of profile files; see `profiles/host/` and `profiles/modem/`.

## Layout

- `crates/diagbuddy`: the daemon: DCI supervisor, stream server, status file, profiles.
- `crates/dcicore`: DCI client library (kernel `/dev/diag` and diag-router transports, codec, HDLC framing). std only. Its `tests/common/fake_router.rs` is a fake router for tests, self-contained so other crates can include it with `#[path]`.
- `profiles/`: host and modem profiles the daemon merges at start.

## Build and test

```sh
cargo test --workspace
cargo build --release --target aarch64-unknown-linux-musl
```

`.cargo/config.toml` links the musl target with the toolchain's own `rust-lld`, so no cross
gcc is needed (`rustup target add aarch64-unknown-linux-musl`).

## Running

```
diagbuddy validate <file>...
diagbuddy run [--profiles-dir D]... [--host H] [--modem M] [--set key=json]... [--status-file P]
diagbuddy status [--status-file P]
```

`run` reads profiles from `/usr/share/rayhunter/profiles` and `/etc/rayhunter/profiles.d`
unless `--profiles-dir` is given (copy `profiles/` to the device), picks the host and modem
profile that match the device (or `--host` / `--modem`), and writes its status file to
`/var/run/rayhunter/diagbuddy.status.json`. Run it under your init system with respawn; it
logs to stderr.

## Stream clients

The stream (`stream.listen`, default `tcp:127.0.0.1:43555`) serves up to `stream.max_clients` clients at once (default 8, 1 to 64). One more is closed on arrival, counted in `client.refused_connections` and logged. Each client has its own LOG_CONFIG state: its set-mask (op 3), disable (op 0) and get-mask (op 4) see only its own masks, never another client's. The DCI subscription is the union of every client's codes and `default_codes`, capped at 4096; a request that would push the union over the cap is refused for that client alone (its reply is unchanged, its codes stay as they were, the others are untouched). A record goes only to the clients whose own masks include its code: a client never receives a code because another client, or `default_codes`, asked for it. A client that stops reading drops its own frames (oldest first) and, after a 5 s write stall, is detached; it never slows the reader or another client. When a client leaves, only the codes no other client still wants are unsubscribed.

## Status file

`diagbuddy run` rewrites `/var/run/rayhunter/diagbuddy.status.json` (`--status-file`) at least every 5 s and whenever something changes, and removes it on a clean stop. Counters marked *lifetime* never go down while the process lives.

| Field | Meaning |
|---|---|
| `pid`, `since` | The daemon's pid and its start time (RFC 3339, UTC). |
| `updated` | When this file was written (RFC 3339, UTC). |
| `profile.host`, `profile.modem` | Resolved profile names. |
| `dci` | `registered` or `retrying`. |
| `dci_registrations` | Successful DCI registrations (lifetime). |
| `dci_failures` | Failed registration attempts plus lost sessions (lifetime). |
| `client_id` | DCI client id of the live session, `null` without one. |
| `transport`, `listen` | Profile transport and the stream address actually bound. |
| `client.attached`, `client.max` | Stream clients attached now, and `stream.max_clients`. |
| `client.refused_connections` | Connections closed on arrival because `max` clients were attached (lifetime). |
| `client.sent`, `client.dropped`, `client.bad_requests` | Frames written, frames dropped, undecodable requests (lifetime, all clients). `sent` is current to within one rewrite. |
| `client.requests` | Client requests answered (lifetime, all clients). |
| `client.refused_commands` | Refused requests by first byte (lifetime). |
| `client.over_cap` | Requests whose log codes, with every other client's and `default_codes`, would exceed 4096; the subscription was left as it was and the client still got its normal reply (lifetime). |
| `clients` | One row per attached client: `id` (counts up from 1, never reused), `codes` (what it was granted), `sent`, `dropped`, `bad_requests`, `requests`, `refused_commands`, `over_cap`. |
| `subscribed` | Log codes in the DCI subscription now (the real one, not what the client last asked for). |
| `records`, `unparsed_bytes` | Log records read from DCI, and bytes that did not parse. |
| `stock_consumers` | Per stock DIAG consumer: `pid_at_start`, `pid_now`, `state` (`present`, `restarted`, `absent`). |
| `events` | The last 20 lines the daemon logged, oldest first, each `{"time", "msg"}`. They hold only the daemon's state, log codes and counters. |

`diagbuddy status` prints the file and exits 0; exits 1 with `not running` when no live diagbuddy wrote it; and exits 3, still printing the file, with the reason on stderr when `updated` is missing or more than 15 s old (a daemon that is running but wedged).

The DCI subscription is capped at 4096 log codes (one whole equipment id, the largest measured). A profile whose `default_codes` alone exceed it is refused. The `kernel-char` transport is accepted by `validate` but refused by `run` as not built yet. After a lost session the retry wait goes back to 1 s only if the session lived at least 60 s; otherwise it doubles, up to 30 s.

## Signals

SIGTERM and SIGINT stop cleanly: unsubscribe, deregister, remove the status file. SIGHUP is ignored. SIGUSR2 is a debug aid: the daemon unsubscribes and deregisters its live DCI session, then registers again at once from the same process, re-subscribing every client's codes. It is not counted in `dci_failures`, and `events` gets a `re-registration requested` line. Use it to see whether the router accepts a second registration from a pid it already knows, which restarting the process never shows.

## License

Apache-2.0; see `LICENSE`. Every source file carries an `SPDX-License-Identifier: Apache-2.0`
header. The only runtime dependencies are `serde` and `serde_json` (MIT OR Apache-2.0).

diagbuddy is a separate program from rayhunter (GPL-3.0) and contains none of its code: the
two communicate only over diagbuddy's TCP stream. This repository includes no OpenWrt
packaging and no rayhunter binaries.
