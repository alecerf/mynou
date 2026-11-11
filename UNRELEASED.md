## Added: Choose the address when starting Mynou

`mynou serve` now takes `--host` and `--port`, which replace the address set in
Settings, Network for that run only: `mynou serve --host 0.0.0.0 --port 9090`
opens Mynou to the network on port 9090, and `--port` alone keeps the saved
host. The saved address does not change and applies again when Mynou starts
without them; Settings, Network says when they are in use. With `--host
0.0.0.0` and no password set, computers on your local network open Mynou
without one for that run, while other networks stay out; a password set in
Settings, Network keeps applying.
