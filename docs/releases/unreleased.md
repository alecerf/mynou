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
