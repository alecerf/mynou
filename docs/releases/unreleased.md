## Added: Set Mynou up from the web interface

Settings and secrets live in Mynou's own storage and are changed from the new
**Settings** view, the API (`GET` and `POST /api/settings`, `GET` and
`POST /api/secrets`) or `mynou settings` and `mynou secrets`. Saved settings
are validated first and apply without a restart; a new listen address applies
at the next start. Secret values are never returned, and a saved secret takes
precedence over the environment variable of the same name.

Every command takes `--dir DIR` (default `$MYNOU_HOME`, else
`~/Library/Application Support/Mynou`); the store is `state/jobs` in that
directory and the API token is `MYNOU_API_TOKEN` in its `.env`
([details](/docs/macos.md#configure-mynou), #359).
