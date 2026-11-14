# stew

Stews: workspaces of repo clones in a box (bento), tracked as branches of one artifacts
repo. See the plan (`agents-in-forests-vy32:plan.md`, section stew) for the design.

- `crates/proto`: the protocol, JSON lines over a unix socket, one request per connection.
- `crates/stew`: the hub (`stew daemon`) and the host CLI, its client. Drivers in
  `src/driver/` (the trait; `host`, `bento` against bento's contracts.md v2); the commands in
  `src/ops.rs`; what runs where a stew lives (clones, the pushed check, stew's commit in
  `notes/`) in `src/scripts/*.sh`, run through the driver's `exec`.
- `crates/box`: the in-box client (bin `stew-box`, installed into boxes as `stew`).

Development (no cargo on the host): `nix develop --no-pure-eval -c just test`, and
`just check` for fmt and clippy. Packages: `nix build .#stew`, `nix build .#stew-box`.

Commands: `stew new [repos…] --title t [--site s] [--branch[=name]] [--base b]`, `open <id>
[--site s]`, `remove|close <id> [--force]`, `start <id>|--all`, `stop <id>`, `enter <id>`,
`exec <id> [--no-start] [-t] -- cmd…`, `title <id> <title>`, `ls [--json]`, `ls --all
[search] [--closed]`, `ls --waiting`, `ls --from <id>`, `daemon [--socket p] [--idle-exit s]`.

Sites (`sites:` in config.yaml; default `desktop: {driver: bento}`, `desktop-host: {driver:
host}`; `site:` picks the default): `{driver: bento, via: ssh mac}`, `{driver: host, root:
~/projects/stews}`.

Units: `systemd/stew.socket` and `stew.service` (socket activation, idle exit), installed by
the home-manager module (`homeManagerModules.default`, `services.stew.enable = true`).

Config: `~/.config/stew/config.yaml` (`artifacts: <remote>`, `scan: <dir>|false`, ...),
the catalog in `~/.config/stew/repos.yaml` (nested groups; a string is a remote).
State: `~/.local/state/stew/state.json`; the artifacts mirror:
`~/.local/share/stew/artifacts.git`; the socket: `$XDG_RUNTIME_DIR/stew/stew.sock`
(`STEW_SOCKET` overrides it for clients).
