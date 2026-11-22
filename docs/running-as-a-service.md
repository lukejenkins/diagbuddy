# Running as a service

diagbuddy is a foreground program. It does not daemonize, write a pid file or rotate logs:
it expects an init system to start it, restart it, and collect its stderr. This repository
does not ship init scripts or packages for any distribution; this page says what a service
definition needs to do.

## What the service must do

- **Run** `diagbuddy run`, with `--profiles-dir` only if the profiles are not in the default
  directories (`/usr/share/rayhunter/profiles`, then `/etc/rayhunter/profiles.d`; a later
  directory wins a name clash). Add `--host` / `--modem` if board matching does not work on
  your router.
- **Respawn** it when it exits. diagbuddy retries a lost or refused DCI registration itself,
  with backoff (1 s, doubling to 30 s), so an exit is a setup error (bad profile, unwritable
  status file, address in use) or a crash, not a transient DIAG problem. A respawn delay of a
  few seconds and a cap on rapid restarts keep a setup error from looping hot.
- **Stop** it with SIGTERM, and wait for it to exit: it unsubscribes and deregisters first.
  A SIGKILL skips that and leaves the status file behind (`status` then reports `not
  running`, because the pid in it is gone); on the tested router the next start registered
  again normally.
- **Send stderr** to the system log. Every line is also kept in the status file's `events`
  (the last 20), so a short log buffer still leaves a trail.
- **Order** it after the stock DIAG consumers and the diag-router are up. Starting first is
  not harmful (it retries), but `stock_consumers` records the pid it saw at start, so a
  consumer that starts later is reported as `restarted` rather than `present`.
- **Start** clients (rayhunter) after it, or let them retry.

On OpenWrt, that is a procd instance with `respawn`, `stdout`/`stderr` logging and a
`term_timeout` of a few seconds; on a plain sysvinit or busybox system, an `inittab`
`respawn` line or a small supervisor does the same.

## Host quirks to check

Some routers do not run init scripts the usual way. On the tested router, `/etc` is an
overlay mounted by an early boot script after the init system has already listed its
startup links, so a startup link added at install time is never run at boot. The host
profile's `evidence.boot_start` note records how boot start was made to work there. Before
relying on boot start on a new router, reboot it once and confirm with `diagbuddy status`
that the daemon came up by itself, then record what you found in that host's profile.

## Health checks

`diagbuddy status` is meant for scripts:

| Exit | Meaning |
|---|---|
| 0 | A live diagbuddy wrote the status file, updated within the last 15 s. |
| 1 | `not running`: no status file, or the pid in it is not a running diagbuddy. |
| 3 | Running, but the file is stale or has no valid `updated` stamp: the daemon may be wedged. The file is still printed, with the reason on stderr. |

A watchdog that restarts the service on exit 3 for more than a minute or so is reasonable;
exit 1 while the service is meant to be up means the init system has not restarted it.
