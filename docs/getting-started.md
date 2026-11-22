# Getting started

This walks from a source checkout to a running stream on a supported router, with
rayhunter reading it. It assumes the router is one of the hosts in the README's tested
hardware table, that you have a root shell on it (ssh or adb), and that you can copy files
to it.

## 1. Build

On your workstation:

```sh
rustup target add aarch64-unknown-linux-musl
cargo test --workspace
cargo build --release --target aarch64-unknown-linux-musl
```

The device binary is `target/aarch64-unknown-linux-musl/release/diagbuddy`. It is static
(`.cargo/config.toml` links it with the toolchain's own `rust-lld`), so it needs no library
on the router and no cross gcc on the workstation. Check with `file`: it should say
`statically linked`.

## 2. Copy it to the router

Copy the binary and the whole `profiles/` directory. Any writable location works; this
guide uses `/tmp/diagbuddy` for a first try, which does not survive a reboot.

```sh
# on the workstation (adb push or scp, whichever the router offers)
scp target/aarch64-unknown-linux-musl/release/diagbuddy root@ROUTER:/tmp/diagbuddy/
scp -r profiles root@ROUTER:/tmp/diagbuddy/
```

For an installed setup, put the binary in `/usr/bin` and the profiles in
`/usr/share/rayhunter/profiles` (or `/etc/rayhunter/profiles.d` for local additions); those
are the directories `run` reads when `--profiles-dir` is not given.

## 3. Validate the profiles

On the router:

```sh
cd /tmp/diagbuddy
./diagbuddy validate profiles/host/*.json profiles/modem/*.json
```

Each file prints `ok` or the first problem in it. Exit status 0 means every file passed.

## 4. Run it in the foreground

```sh
./diagbuddy run --profiles-dir /tmp/diagbuddy/profiles
```

With no `--host`, diagbuddy reads `/tmp/sysinfo/board_name` and picks the host profile whose
`match.board` equals it; the host profile names its modem profile. Name them explicitly with
`--host <stem> --modem <stem>` when the board file is missing or matches more than one
profile. See [profiles.md](profiles.md).

It logs to stderr. A healthy start looks like:

```
serving tcp:127.0.0.1:43555 for <host>/<modem>
DCI registered as client 1 on the diag-router
```

If you see `DCI registration failed: …; retrying in …` instead, see
[troubleshooting.md](troubleshooting.md).

## 5. Check the status

From a second shell:

```sh
./diagbuddy status
```

It prints the status file (`/var/run/rayhunter/diagbuddy.status.json`) and exits 0. Look for
`"dci": "registered"`, a non-null `client_id`, and every `stock_consumers` entry in state
`present`. `subscribed` is empty until a client asks for codes, unless the profile sets
`default_codes`.

## 6. Point rayhunter at it

rayhunter reads diagbuddy with its external stream device. In rayhunter's `config.toml`:

```toml
device = "external"
diag_stream = "127.0.0.1:43555"
```

This needs a rayhunter build that has the external stream device. Start rayhunter after
diagbuddy, or let it retry: a rayhunter build with stream reconnect attaches again by
itself when diagbuddy restarts.

Once rayhunter is up, `diagbuddy status` shows `client.attached: 1`, one `clients` row with
the codes rayhunter asked for, and those codes in `subscribed`. `records` climbs as the modem
logs them. A code only arrives when the modem actually emits it: an idle unit, or a firmware
that does not log a code at all, gives no records for that code (the README's tested
hardware table notes what each tested firmware emits).

## 7. Stop it

Ctrl-C (SIGINT) or SIGTERM stops cleanly: diagbuddy unsubscribes, deregisters from the DIAG
stack and removes its status file. Next, run it as a service:
[running-as-a-service.md](running-as-a-service.md).
