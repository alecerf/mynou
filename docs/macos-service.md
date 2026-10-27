# Run Mynou under launchd

`mynou service` installs a LaunchAgent owned by your macOS account. launchd
starts the native executable directly, with no shell wrapper, Docker or helper
daemon. It starts after **account login**, not before login at system boot, and
stops on logout. Keep the owning account logged in for unattended operation.
The service has that account's filesystem permissions; it does not run as root.

## Prepare the account

Install the [checksum-verified release](macos.md#download-and-verify) at a
stable absolute path, for example `~/.local/bin/mynou`. Keep the binary and
data directory outside Desktop, Documents and Downloads: macOS privacy controls
can suspend a launchd process before it reaches Mynou code when its executable
or inputs are in protected folders. Grant any necessary volume/privacy access
to the deployed executable yourself in System Settings. Do not disable macOS
privacy controls or give the service root privileges.

For a new installation, first start a foreground `mynou serve` once: it creates
the private data directory, and its web interface sets Mynou up (see [Configure Mynou](macos.md#configure-mynou)).

Configure the library and downloads before installation. Every path must
resolve to an existing absolute directory without symlinks; relative settings
resolve against the absolute data directory.
Provision your chosen directories explicitly with the owning account and mount
media volumes first. Installation and supervised startup refuse missing roots
instead of creating local replacements for absent volumes. Do not copy the
owner's real inventory or credentials into an example configuration.

The data directory identifies the installation: it must be an existing
directory owned by the account, private (mode 0700) and without symlinked path
components. Save credentials in Mynou's storage from the Settings view: the
service reads them there. Credentials exported only
in your terminal are not copied into the service.

## Install and inspect

```sh
mynou service --action install
mynou service --action status
curl -s http://127.0.0.1:8787/readyz
```

The default label is `org.mynou.automation`; `--label org.mynou.NAME` selects a
separate registration. Never install two registrations for the same stores.
The listener and existing exclusive store locks reject a second owner.
`service status` reports registration, loaded state and process presence.
Process presence alone is not readiness: `/readyz` reports it, and the
overview of the web interface shows startup progress and diagnostic state. Verify these after
installation, upgrades, crashes and volume changes.

Installation writes only the account's
`~/Library/LaunchAgents/org.mynou.automation.plist` and a private
`.mynou-service/` directory in the data directory. It refuses to replace a
different registration or operate on another data directory's job. Repeating
an identical install does not restart an already running process. An unavailable
login session is an error, distinct from an absent job.

The private plist holds no credentials: its arguments contain only the binary,
internal `service-run` command and data directory, and its environment only
`HOME`. Management commands return fixed fields and suppress raw `launchctl`
output. Settings and saved secrets apply while the service runs; uninstall
leaves the data directory untouched.

Persisted protected VPN intent is handled by the existing native startup gate.
Installing a service neither enables nor disables that protection. Import and
enable the required profile using the [VPN operator controls](wireguard-operator.md)
before enabling automation, and verify its status after restart. Source/IRC
protection remains the separate scope of #337; service installation does not
extend the delivered torrent protection boundary.

## Stop, restart and uninstall

```sh
mynou service --action stop
mynou service --action start
mynou service --action uninstall
```

Stop sends an authenticated shutdown fenced to the PID reported by launchd,
waits up to 120 seconds for work to finish, then unloads the registration.
A different API process refuses the shutdown. An authentication failure,
unreachable API or stop timeout preserves registration and data and reports an
error; it does not force-kill the process. Investigate the API and pending work
before retrying. Crash recovery uses the existing durable journals.

launchd restarts unsuccessful exits, throttled to at most one launch per
60 seconds. A clean API shutdown exits successfully and does not trigger a
restart. `start` loads an unloaded registration or starts a stopped loaded job;
it does not restart a live process. Stop and uninstall are repeatable. Uninstall
stops and removes **only the plist**: settings, credentials, stores, logs,
downloads, media and backups remain. An account login will load a still-installed
plist again, so uninstall it to disable future login startup.

## Logs and recovery

Supervised mode discards stdout/stderr and writes only timestamped fixed codes
to `.mynou-service/events.log`, rotating into `events.previous.log`. Both files
are private and each is bounded to 64 KiB. Raw provider responses, credentials,
private paths and raw error strings do not enter these logs.

| Code or symptom | Next action |
| --- | --- |
| `media_volume_unavailable` / `download_volume_unavailable` | Mount the configured volume; check the account's directory access, then start again. |
| `state_directory_unavailable` | Restore the data directory's `state/` and account access; do not initialize an empty substitute for missing state. |
| `runtime_failed` | Check `doctor`, the authenticated status API and filesystem permissions. After a graceful stop, run `serve` interactively if fuller local diagnostics are needed. Keep that output private. |
| `storage_migration` | Inspect startup progress with `status`; leave the process running while it migrates. |
| `stopped_cleanly` | Work drained and the supervisor returned successfully. |
| Loaded process but no log or API | Check the binary and data directory paths and macOS privacy permissions; a process blocked by the OS before main cannot write a Mynou diagnostic. |

Volume checks run at startup, before storage opens. Runtime import/transfer
errors remain visible through the existing API; a service registration does
not make detached disks reliable. Remount the same volume and inspect state
before resuming. Do not create replacement directories at a missing mount.

For a coherent backup: stop, verify `service status` is unloaded, then back up
the data directory (all stores, including settings, secrets and VPN policy/profile material),
downloads and media according to your retention policy. Keep credentials and
backups private. Uninstall before changing the binary path.
Mynou [updates itself](macos.md#updates) in place. It restarts within the same
process, so the registration and its binary path stay valid, and a version that
cannot start is rolled back to the previous executable and state. For a manual
update, download and checksum-verify the new binary, preserve the previous
executable, install it at the same stable path, reinstall/start, then verify
health, pending work and VPN intent. No work PR changes the package version.

For rollback, stop and uninstall the new registration, restore a compatible
binary **and its coherent pre-upgrade state backup** when formats changed,
reinstall, and verify again. Do not overwrite newer media or payloads or delete
data to make an older binary open it. Rehearse with synthetic data before a real
cutover. The existing stack stays running until the separately authorized #344
acceptance window; this feature does not perform that cutover.

## Native validation

Unit tests cover literal environment handling, XML escaping and native plist
lint, missing-volume refusal, bounded private logs and PID-fenced shutdown.
On a logged-in native macOS account, run the opt-in fixture:

```sh
cargo build --offline --locked
python3 .github/scripts/launchd_smoke.py target/debug/mynou
```

It copies the executable into a temporary directory, uses synthetic local-only
configuration and an isolated HOME with a unique `org.mynou.fixture.PID` label,
and checks install/idempotency, second-owner refusal, crash restart, graceful
stop/start, missing-volume refusal, uninstall and payload preservation. It
unloads only that fixture label on failure. It neither uses the default label
nor connects to real media services. GUI-less CI runs the deterministic unit
fixtures; the real launchd check is explicit native operator evidence.
