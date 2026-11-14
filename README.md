# stew

Stews: workspaces of repo clones in a box (bento), tracked as branches of one artifacts
repo. See the plan (`agents-in-forests-vy32:plan.md`, section stew) for the design.

- `crates/proto`: the protocol, JSON lines over a unix socket, one request per connection.
- `crates/stew`: the hub (`stew daemon`) and the host CLI, its client. Drivers in
  `src/driver/` (the trait; `host`, `bento` against bento's contracts.md v2); the commands in
  `src/ops.rs`; what runs where a stew lives (clones, the pushed check, stew's commit in
  `notes/`) in `src/scripts/*.sh`, run through the driver's `exec`.
- `crates/box`: the in-box client (bin `stew-box`, installed into boxes as `stew`, and in the
  host package as `libexec/stew/stew`, first on host-driver stews' `PATH`): `repos`, `info`,
  `new … [--brief f [--start]]`, `agent <id> "<task>"`, `ls --from .`, `skill`. It talks to
  `STEW_HUB` (`unix:<socket>`) with its stew's id (from `notes/stew.yaml` walking up) and
  `STEW_TOKEN`; the daemon takes a request as that stew's only with its token, and only for
  those calls.
- `src/runtime.rs`: the agent runtime, hc. The hub's core (`stew-hc.service`,
  `HC_SESSION=stew`) gets each stew as a machine, via `stew exec <id> --no-start -- sh -c`;
  a host stew has its own hc session `stew-<id>`. `src/agent.rs`: the agents' command lines.
- `skills/stew/SKILL.md`: the stew skill (`stew skill` prints it; `stew skills` assembles
  `~/.local/share/stew/skills/` with hc's stub and the artifacts repo's `main:skills/`).

Development (no cargo on the host): `nix develop --no-pure-eval -c just test`, and
`just check` for fmt and clippy. The dev shell has hc; the runtime tests run their own hub
core in their own dirs. Packages: `nix build .#stew`, `.#stew-box`, `.#herdr-core`.

Commands: `stew new [repos…] --title t [--site s] [--branch[=name]] [--base b] [--brief f
[--start] [--agent a] [--model m]]`, `agent <id> [--agent a] [--model m] "<task>"`, `attach
<id>/<name>`, `skills`, `skill`, `open <id>
[--site s]`, `remove|close <id> [--force]`, `start <id>|--all`, `stop <id>`, `enter <id>`,
`exec <id> [--no-start] [-t] -- cmd…`, `title <id> <title>`, `ls [--json]`, `ls --all
[search] [--closed]`, `ls --waiting`, `ls --from <id>`, `daemon [--socket p] [--idle-exit s]`.

Sites (`sites:` in config.yaml; default `desktop: {driver: bento}`, `desktop-host: {driver:
host}`; `site:` picks the default): `{driver: bento, via: ssh mac}`, `{driver: host, root:
~/projects/stews}`.

Units: `systemd/stew.socket` and `stew.service` (socket activation, idle exit), and
`stew-hc.service` (the hub's hc core, always on), installed by the home-manager module
(`homeManagerModules.default`, `services.stew.enable = true`, which also installs hc).

Config: `~/.config/stew/config.yaml` (`artifacts: <remote>`, `scan: <dir>|false`, `agent:`,
`model:`, `runtime: hc|none`, ...),
the catalog in `~/.config/stew/repos.yaml` (nested groups; a string is a remote).
State: `~/.local/state/stew/state.json`; the artifacts mirror:
`~/.local/share/stew/artifacts.git`; the socket: `$XDG_RUNTIME_DIR/stew/stew.sock`
(`STEW_SOCKET` overrides it for clients).
