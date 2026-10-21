# Run Mynou under launchd

`mynou service` installs a LaunchAgent owned by your macOS account. launchd
starts the native executable directly, with no shell wrapper, Docker or helper
daemon. It starts after **account login**, not before login at system boot, and
stops on logout. Keep the owning account logged in for unattended operation.
The service has that account's filesystem permissions; it does not run as root.

## Prepare the account

Install the [checksum-verified release](macos.md#download-and-verify) at a
stable absolute path, for example `~/.local/bin/mynou`. Keep the binary and
configuration outside Desktop, Documents and Downloads: macOS privacy controls
can suspend a launchd process before it reaches Mynou code when its executable
or inputs are in protected folders. Grant any necessary volume/privacy access
to the deployed executable yourself in System Settings. Do not disable macOS
privacy controls or give the service root privileges.

For a new configuration, create a private directory first:

```sh
mkdir -p "$HOME/.config/mynou"
chmod 700 "$HOME/.config/mynou"
mynou init --config "$HOME/.config/mynou/mynou.json"
```

Configure the library, downloads and stores before installation. Every path
must resolve to an existing absolute directory without symlinks; relative
configuration values resolve against the absolute configuration directory.
Provision your chosen directories explicitly with the owning account and mount
media volumes first. Installation and supervised startup refuse missing roots
instead of creating local replacements for absent volumes. Do not copy the
owner's real inventory or credentials into an example configuration.

The configuration and its adjacent `.env` must be regular mode-0600 files,
owned by the account, without hardlinks or symlinked path components. Put all
required credentials in `.env`, including the configured API token. Its syntax
is **literal unquoted `NAME=value`**, one entry per line, uppercase names, at
most 64 KiB total and 4096 bytes per value. Blank lines and `#` comments are
accepted. Quotes, dollars and backticks are literal; nothing is evaluated.
Duplicate names, control characters, loader injection variables and overrides
of HOME/PATH/TMPDIR are rejected. Credentials exported only in your terminal
are not copied into the service.

## Install and inspect

```sh
mynou doctor --config "$HOME/.config/mynou/mynou.json"
mynou service --action install --config "$HOME/.config/mynou/mynou.json"
mynou service --action status --config "$HOME/.config/mynou/mynou.json"
mynou healthcheck --config "$HOME/.config/mynou/mynou.json"
mynou status --config "$HOME/.config/mynou/mynou.json"
mynou vpn --action status --config "$HOME/.config/mynou/mynou.json"
```

The default label is `org.mynou.automation`; `--label org.mynou.NAME` selects a
separate registration. Never install two registrations for the same stores.
The listener and existing exclusive store locks reject a second owner.
`service status` reports registration, loaded state and process presence.
Process presence alone is not readiness: `healthcheck` checks readiness and
`status` shows startup progress and diagnostic state. Verify these after
installation, upgrades, crashes and volume changes.

Installation writes only the account's
`~/Library/LaunchAgents/org.mynou.automation.plist` and a private
`.mynou-service/` directory adjacent to the configuration. It refuses to replace
a different registration or operate on another configuration's job. Repeating
an identical install does not restart an already running process. An unavailable
login session is an error, distinct from an absent job.

The **private plist contains a snapshot of `.env` credentials** in its launchd
environment; its arguments contain only the binary, internal `service-run`
command and configuration path. Never publish the plist, `.env` or raw
`launchctl print` output: the latter includes the environment. Management
commands return fixed fields and suppress that raw output. Changing secrets
requires stopping, uninstalling and reinstalling the registration, so the
snapshot is refreshed; uninstall does not delete `.env`.

Persisted protected VPN intent is handled by the existing native startup gate.
Installing a service neither enables nor disables that protection. Import and
enable the required profile using the [VPN operator controls](wireguard-operator.md)
before enabling automation, and verify its status after restart. Source/IRC
protection remains the separate scope of #337; service installation does not
extend the delivered torrent protection boundary.

## Stop, restart and uninstall

```sh
mynou service --action stop --config "$HOME/.config/mynou/mynou.json"
mynou service --action start --config "$HOME/.config/mynou/mynou.json"
mynou service --action uninstall --config "$HOME/.config/mynou/mynou.json"
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
stops and removes **only the plist**: configuration, credentials, stores, logs,
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
| `state_directory_unavailable` | Restore the configured store directory and account access; do not initialize an empty substitute for missing state. |
| `runtime_failed` | Check `doctor`, the authenticated status API and filesystem permissions. After a graceful stop, run `serve` interactively if fuller local diagnostics are needed. Keep that output private. |
| `storage_migration` | Inspect startup progress with `status`; leave the process running while it migrates. |
| `stopped_cleanly` | Work drained and the supervisor returned successfully. |
| Loaded process but no log or API | Check binary/config paths and macOS privacy permissions; a process blocked by the OS before main cannot write a Mynou diagnostic. |

Volume checks run at startup, before storage opens. Runtime import/transfer
errors remain visible through the existing API; a service registration does
not make detached disks reliable. Remount the same volume and inspect state
before resuming. Do not create replacement directories at a missing mount.

For a coherent backup: stop, verify `service status` is unloaded, then back up
the configuration, `.env`, all stores (including VPN policy/profile material),
downloads and media according to your retention policy. Keep credentials and
backups private. Uninstall before changing the binary path or `.env` snapshot.
Download and checksum-verify the new binary, preserve the previous executable,
install at the same stable path, reinstall/start, then verify health, pending
work and VPN intent. No work PR changes the package version.

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
