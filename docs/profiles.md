# Profiles

Everything diagbuddy needs to know about one router and its modem lives in two JSON files:
a **host** profile (the router: how to recognise it, where it keeps data) and a **modem**
profile (the DIAG stack: transport, session mode, stock consumers). Supporting a new
router is writing that pair, not changing code.

## Layout

Each profile directory holds up to three subdirectories:

```
<dir>/host/<host>.json
<dir>/modem/<modem>.json
<dir>/combo/<host>--<modem>.json      optional
```

A file's stem is its name (`host/my-router.json` is the host `my-router`). `run` reads every
`--profiles-dir` in order, or `/usr/share/rayhunter/profiles` then
`/etc/rayhunter/profiles.d` when none is given. When two directories hold the same stem,
the later one wins, so local files in `/etc/rayhunter/profiles.d` replace packaged ones
without editing them.

## Resolution

1. **Host.** `--host <stem>`, or with `--host auto` (the default) the one host profile whose
   `match.board` equals the first line of `/tmp/sysinfo/board_name`. No match, or more than
   one, is an error that names the board and the candidates.
2. **Modem.** `--modem <stem>`, or with `--modem auto` (the default) the host profile's
   `modem` key.
3. **Merge.** Starting from an empty object, diagbuddy applies, in order: the modem file,
   the host file (without its `match` and `modem` keys), the `combo/<host>--<modem>.json`
   file if there is one, then every `--set`. Each step is a JSON Merge Patch (RFC 7386):
   - objects merge key by key;
   - any other value, **arrays included, replaces** what was there;
   - `null` deletes the key.
4. **Check.** The merged object must be a complete profile (below) and pass the
   consistency rules; otherwise `run` exits 1 naming `<host>/<modem>` and the problem.

`--set key=json` builds a patch from a dotted key and a JSON value, and may be repeated:

```sh
diagbuddy run --set 'stream.listen="tcp:127.0.0.1:43600"' \
              --set 'diag.default_codes=["0xB0C0","0xB0EC"]'
```

The value must be JSON, so a string needs its quotes (`"dci"`, not `dci`).

## Keys

A merged profile has these keys. Unknown keys are refused at every level, so a typo fails
`validate` instead of being ignored.

| Key | Required | Meaning |
|---|---|---|
| `spec_version` | yes | Must be `1`. |
| `diag.transport` | yes | How diagbuddy reaches the DIAG stack: `diag-router` (the userspace diag-router's socket), `kernel-char` (a DIAG character device; accepted by `validate`, refused by `run` as not built yet), or `serial`. |
| `diag.session` | yes | `dci`: a DCI client subscription beside the stock consumers. `exclusive`: diagbuddy owns the port. |
| `diag.default_codes` | no | Log codes to keep subscribed with no client attached, as `"0x"` + 1 to 4 hex digits. They are never delivered to a client that did not ask for them. Default `[]`. |
| `diag.multi_radio_header` | no | Put the 8-byte `0x98` multi-radio header in front of every stream frame. Default `false`; see [stream-protocol.md](stream-protocol.md). |
| `diag.stock_consumers` | no | Stock processes that use DIAG, each `{"name", "pidof"}` (`pidof` is the process name to look up). diagbuddy only watches them and reports their state; it never signals them. |
| `storage.recordings_path` | yes | Where the router keeps recordings, for clients that read it from the profile. |
| `stream.listen` | no | `tcp:<loopback ip>:<port>` or `unix:<path>`. Default `tcp:127.0.0.1:43555`. A unix socket is created mode 0600. |
| `stream.max_clients` | no | Stream clients attached at once, 1 to 64. Default 8. |
| `evidence` | no | Free-form: what was measured on this hardware (below). Not read by the daemon. |

Host-only keys, refused in a modem or combo file:

| Key | Meaning |
|---|---|
| `match.board` | The router's board name, the first line of `/tmp/sysinfo/board_name`. |
| `modem` | The modem profile stem this host uses with `--modem auto`. |

Consistency rules checked on the merged profile:

- `session: "exclusive"` with a non-empty `stock_consumers` is refused: an exclusive
  session would fight the stock consumer.
- `transport: "serial"` requires `session: "exclusive"`, and `session: "dci"` requires
  `diag-router` or `kernel-char`.
- `default_codes` may hold at most 4096 distinct codes (the subscription cap).

## Evidence

`evidence` is how a profile says what is known, not just what is configured. Each entry is
keyed by what was tested and holds:

```json
"diag_coexistence": {
  "verdict": "🟢",
  "date": "YYYY-MM-DD",
  "note": "what was run, what was measured, the numbers"
}
```

Use 🟢 for measured and working, 🟡 for working with a known limit (say what it is in the
note), 🔴 for measured and not working. The keys the tested profiles use are
`diag_transport`, `diag_coexistence`, `boot_start` and `install`. Write the note so that
someone else could repeat the measurement.

## Writing a pair for a new router

1. **Find the board name**: `head -1 /tmp/sysinfo/board_name` on the router.
2. **Find the DIAG path.** Is there a userspace diag-router (a process of that name, and an
   abstract unix socket it listens on), or only a kernel DIAG device? Only `diag-router`
   with `dci` is built today.
3. **List the stock consumers**: processes that hold DIAG open (`ls -l /proc/*/fd` pointing
   at the DIAG device or the router socket). Put each in `stock_consumers` so their state is
   reported.
4. **Write the modem file**, starting from the tested one in `profiles/modem/`. Keep
   `default_codes` empty unless something must stay subscribed with no client.
5. **Write the host file**: `match.board`, `modem`, `spec_version`,
   `storage.recordings_path`.
6. `diagbuddy validate` both, then `diagbuddy run` and check `diagbuddy status`:
   `dci: registered`, stock consumers `present`.
7. **Measure coexistence before trusting it.** While a client streams a busy set of codes,
   check that every stock consumer keeps its pid, and that the stock DIAG path (for example
   the USB DIAG port, if the router exposes one) still answers a simple request at its usual
   latency. Record the numbers in `evidence`.
8. Put anything that applies only to this host + modem pair in a `combo` file rather than
   in either half.
