# stew

Stews: workspaces of repo clones in a box (bento), tracked as branches of one artifacts
repo. See the plan (`agents-in-forests-vy32:plan.md`, section stew) for the design.

- `crates/proto`: the protocol, JSON lines over a unix socket, one request per connection.
- `crates/stew`: the hub (`stew daemon`) and the host CLI, its client.
- `crates/box`: the in-box client (bin `stew-box`, installed into boxes as `stew`).

Development (no cargo on the host): `nix develop --no-pure-eval -c just test`, and
`just check` for fmt and clippy. Packages: `nix build .#stew`, `nix build .#stew-box`.

Config: `~/.config/stew/config.yaml` (`artifacts: <remote>`, `scan: <dir>|false`, ...),
the catalog in `~/.config/stew/repos.yaml` (nested groups; a string is a remote).
State: `~/.local/state/stew/state.json`; the artifacts mirror:
`~/.local/share/stew/artifacts.git`; the socket: `$XDG_RUNTIME_DIR/stew/stew.sock`
(`STEW_SOCKET` overrides it for clients).
