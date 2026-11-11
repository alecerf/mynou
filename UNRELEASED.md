## Added: Choose the address when starting Mynou

`mynou serve` now takes `--host` and `--port`, which replace the address set in
Settings, Network for that run only: `mynou serve --host 0.0.0.0 --port 9090`
opens Mynou to the network on port 9090, and `--port` alone keeps the saved
host. The saved address does not change and applies again when Mynou starts
without them; Settings, Network says when they are in use. With `--host
0.0.0.0` and no password set, computers on your local network open Mynou
without one for that run, while other networks stay out; a password set in
Settings, Network keeps applying.

## Changed: Data in ~/.local/share/mynou

Mynou now keeps its data in `~/.local/share/mynou`, or in
`$XDG_DATA_HOME/mynou` when that variable is set, instead of
`~/Library/Application Support/Mynou`, which it no longer reads. To keep an
existing installation, stop Mynou and move that folder to
`~/.local/share/mynou`. A launchd service names its folder with `--dir`, so it
keeps the one it was installed with. `--dir` and `MYNOU_HOME` still choose
another place.
