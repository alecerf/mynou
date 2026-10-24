## Added: Set Mynou up from the web interface

Settings and secrets live in Mynou's own storage and are changed from the new
**Settings** view, the API (`GET` and `POST /api/settings`, `GET` and
`POST /api/secrets`) or `mynou settings` and `mynou secrets`. Saved settings
are validated first and apply without a restart; a new listen address applies
at the next start. Secret values are never returned, and a saved secret takes
precedence over the environment variable of the same name.

The first visit of the web interface creates the access token and shows it
once; Mynou keeps only its SHA-256 digest. It is offered only from this
machine, to a loopback listener; `mynou token --reset`, while Mynou is
stopped, replaces a lost token. Commands that reach the running service read
it from `MYNOU_API_TOKEN`. Every command takes `--dir DIR` (default
`$MYNOU_HOME`, else `~/Library/Application Support/Mynou`), whose `state/jobs`
holds the storage ([details](/docs/macos.md#configure-mynou), #359, #361).

## Added: Automatic updates

Like Plex Media Server, Mynou updates itself. Once a day it checks the latest
release, then downloads the new executable and checks it against
`SHA256SUMS`. Once no request is in progress, it stops cleanly, backs up
`state/` and the previous executable under `updates/` in the data directory,
and restarts on the new version within the same process. If the new version
cannot start, Mynou restores that backup and the previous executable. The
dashboard offers **Install now**, `mynou update` installs on demand, and
`updates.automatic: false` turns automatic updates off
([details](/docs/macos.md#updates), #363).
