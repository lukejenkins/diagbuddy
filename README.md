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

## Documentation

- [Getting started](docs/getting-started.md): build, copy to the router, first `validate` /
  `run` / `status`, and pointing rayhunter at the stream.
- [Running as a service](docs/running-as-a-service.md): what an init script must do, and
  health checks.
- [Profiles](docs/profiles.md): host and modem profiles, how they merge, `--set`,
  `evidence`, and writing a pair for a new router.
- [Stream protocol](docs/stream-protocol.md): framing, the LOG_CONFIG requests each client
  sends, the 4096-code cap; enough to write a client that is not rayhunter.
- [Troubleshooting](docs/troubleshooting.md): `diagbuddy status` and its exit codes, the
  `events` lines, a client that gets nothing, SIGUSR2, and the status file reference.

## Usage

```
diagbuddy validate <file>...
diagbuddy run [--profiles-dir D]... [--host H] [--modem M] [--set key=json]... [--status-file P]
diagbuddy status [--status-file P]
```

The stream listens on `tcp:127.0.0.1:43555` by default and serves up to 8 clients at once,
each with its own log mask. For rayhunter, set `device = "external"` and
`diag_stream = "127.0.0.1:43555"` in its `config.toml`.

## License

Apache-2.0; see `LICENSE`. Every source file carries an `SPDX-License-Identifier: Apache-2.0`
header. The only runtime dependencies are `serde` and `serde_json` (MIT OR Apache-2.0).

diagbuddy is a separate program from rayhunter (GPL-3.0) and contains none of its code: the
two communicate only over diagbuddy's TCP stream. This repository includes no OpenWrt
packaging and no rayhunter binaries.
