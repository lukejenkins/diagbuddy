# Troubleshooting

Start with `diagbuddy status`. Its exit status says whether a daemon is running at all, and
the file it prints holds the counters and the last 20 log lines (`events`).

## `diagbuddy status`

| Exit | Means | Do |
|---|---|---|
| 0 | Running and the status is fresh. | Read the fields below. |
| 1 `not running` | No status file, or its pid is not a live diagbuddy. | Check the service, or run `diagbuddy run` in the foreground to see why it exits. |
| 3 | Running, but `updated` is more than 15 s old or missing. | The daemon is wedged; `events` holds its last lines. Restart it. |
| 2 | Bad arguments. | `--status-file` takes one path. |

`diagbuddy run` exits 1 for a setup error (no matching profile, a bad profile, a status
file it cannot write, an address it cannot bind), with the reason on stderr, and 2 for bad
arguments.

## The fields to read first

- **`dci`**: `registered` is the normal state. `retrying` means there is no DCI session now;
  `events` says why (see below).
- **`dci_failures`**: refused registrations plus lost sessions over the process lifetime.
  A small number after boot is normal if diagbuddy started before the diag-router. A
  steadily climbing number is a problem.
- **`stock_consumers`**: one row per consumer named in the profile.
  - `present`: the pid seen at start is still running. This is what you want.
  - `restarted`: a different pid runs that program now, or it was not running when
    diagbuddy started and runs now. diagbuddy never stops a stock consumer, so a restart
    came from something else; if they line up in time with diagbuddy starts, report it.
  - `absent`: not running now.
- **`subscribed`** against each `clients[].codes`: `subscribed` is what the DIAG stack was
  actually asked for. A client whose codes are missing from it was probably refused for
  the cap (its `over_cap` went up).
- **`records`**: log records received from DIAG. It only climbs while the modem emits a
  subscribed code. Zero with codes subscribed usually means the modem is idle or this
  firmware does not log those codes, not that diagbuddy is broken.

## `events` lines

| Line | Means |
|---|---|
| `serving <addr> for <host>/<modem>` | Started; the stream is bound. |
| `DCI registered as client N on the <transport>` | A session is up. |
| `DCI registration failed: <error>; retrying in <wait>` | The DIAG stack refused or did not answer. The wait doubles from 1 s up to 30 s. Check that the diag-router runs and that the profile's transport is right for this router. |
| `DCI lost after <time> (<why>); retrying in <wait>` | A live session ended. The wait goes back to 1 s if the session had lived at least 60 s; otherwise it doubles. Repeated short sessions mean the stack keeps dropping the client. |
| `stock consumer <name> (<pidof>) is not running` | At start, a consumer named in the profile was absent. |
| `client N attached; M attached` | A stream client connected. |
| `refused N stream connection(s): M clients attached, the limit …` | `stream.max_clients` were already attached. Raise it in a combo file or with `--set`, or find the client that reconnects without closing. |
| `re-registration requested: …` | A SIGUSR2 was handled (below). |
| `stopped` | Clean stop. |

## A client gets nothing

1. Is it attached? It should have a row in `clients`.
2. Did it set masks? Its `codes` must list what it wants; `requests` counts its answered
   requests and `bad_requests` / `refused_commands` its rejected ones. A `bad_requests`
   count usually means a framing or FCS mistake on the client side; see
   [stream-protocol.md](stream-protocol.md).
3. Is `over_cap` non-zero for it? Then the union of all clients' codes is at the 4096 cap.
4. Is `dropped` climbing for it? It reads too slowly; diagbuddy drops its oldest frames
   rather than slow anyone else down.
5. Is the code being logged at all? Attach a second client asking for a code that is always
   busy on this modem, and see whether its `sent` climbs.

## A reader refuses the frames

If a reader that works on a DIAG serial port rejects diagbuddy's frames, check
`diag.multi_radio_header`. It is off by default; turn it on only for a reader that expects
the 8-byte `0x98` header in front of each log packet.

## SIGUSR2: re-registering in place

```sh
kill -USR2 $(pidof diagbuddy)
```

diagbuddy unsubscribes and deregisters its live DCI session, then registers again at once
from the same process and subscribes every client's codes again. Clients stay attached;
records pause only for the moment between the two. `dci_registrations` goes up by one and
`dci_failures` does not; `events` gets
`re-registration requested: client N deregistered after T; registering again`. With no live
session it only logs that there was nothing to drop.

It is a debug aid. A process restart always registers from a new pid; SIGUSR2 shows whether
the DIAG stack accepts a second registration from a pid it already knows, which matters for
any recovery that does not restart the process.

SIGHUP and SIGUSR1 are caught and do nothing; neither stops the daemon.

## Status file reference

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
