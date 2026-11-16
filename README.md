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
- `src/runtime.rs`: the agent runtime, hc. The hub's core (`stew-hc.service`, hc's default
  session, the one herdr-app uses) gets each stew as a machine, via `stew exec <id>
  --no-start -- sh -c`, with one herdr space: a shell `stew-shell` at the root labelled
  `herdr.space=stew`/`herdr.tab=shell`, and each agent stew starts a tab of it (`herdr.tab=
  <name>`); a host stew has its own hc session `stew-<id>`. `src/agent.rs`: the agents' command lines.
- `src/transcripts.rs`: sessions export (`src/scripts/export.sh`, run through `exec`: claude's
  jsonl under `~/.claude/projects/`, opencode's `session list`/`export`, only sessions under
  the stew's root) into the hub's checkout of the transcripts repo,
  `<id>/<agent>-<session>.json[l]` plus `.meta.json`; on `stop`, `remove` (fail-closed) and
  hourly (`stew export --all`), committed daily or at once on `stop`/`remove`/`--push`.
- `skills/`: the stew skill (`stew skill` prints it) and hc's stub; `stew skills` assembles
  `~/.local/share/stew/skills/` from these two and the skills of your skills repo (`skills:
  <remote>` in config.yaml: each top-level dir with a `SKILL.md`, copied whole, links left
  out; the hub keeps a shallow checkout in `~/.local/share/stew/skills-repo` and fetches it
  on each `stew skills`, hourly). Boxes get the dir copied in at each boot (never mounted).
  The package carries stew's two in `share/stew/skills`, and
  `services.stew.hostSkills.enable` links them into the host's `~/.claude/skills/` and
  `~/.config/opencode/skills/` (off by default).

Development (no cargo on the host): `nix develop --no-pure-eval -c just test`, and
`just check` for fmt and clippy. The dev shell has hc; the runtime tests run their own hub
core in their own dirs. Packages: `nix build .#stew`, `.#stew-box`, `.#herdr-core`.

Commands: `stew new [repos…] --title t [--site s] [--branch[=name]] [--base b] [--brief f
[--start] [--agent a] [--model m] [--account n]]`, `agent <id> [--agent a] [--model m] [--account n] "<task>"`, `attach
<id>/<name>`, `skills`, `skill`, `open <id>
[--site s]`, `remove|close <id> [--force]`, `start <id>|--all`, `stop <id>`, `enter <id>`,
`exec <id> [--no-start] [-t] -- cmd…`, `export <id>|--all [--push]`, `title <id> <title>`, `ls [--json]`, `ls --all
[search] [--closed]`, `ls --waiting`, `ls --from <id>`, `daemon [--socket p] [--idle-exit s]`.
`--account n` (claude only, to spread usage over subscriptions) starts claude with
`CLAUDE_CODE_OAUTH_TOKEN` from `CLAUDE_ACCOUNT_<N>_TOKEN`, which bento's claude preset sets
per account; its terminal is `claude-<n>-<k>`. None given: `account:` in the config, else
the box's default account. An agent's own helpers (`hc run`) get the box's default.

Wherever a command takes an `<id>`, its short form works too: the four random chars at
its end (`k3fq` for `port-forwarding-k3fq`), unique among tracked stews (a claim skips a
short form in use). Long commands print their steps on stderr (`stew: <id>: …`; the
protocol's `Progress` lines, which the daemon also logs).

Sites (`sites:` in config.yaml; default `desktop: {driver: bento}`, `desktop-host: {driver:
host}`; `site:` picks the default): `{driver: bento, via: ssh mac}`, `{driver: host, root:
~/projects/stews}`.

Units: `stew.socket` and `stew.service` (always on from login, socket-activated too; `services.stew.idleExit` brings back an idle exit), `stew-hc.service`
(the hub's hc core, always on) and `stew-hourly.timer` (`stew export --all`, then `stew
skills`), from `nix/units.nix`, installed by the home-manager module
(`homeManagerModules.default` = `.stew`, `services.stew.enable = true`, which also installs
hc). They name store paths only: ExecStart, and PATH from stew's own tools plus
`services.stew.agentPackages` (what host stews' agents get: claude, opencode, nix and the usual
command-line tools, by default) and `extraPackages` (bento's package when `programs.bento` is
enabled; a git helper the git config names without a path). `nix flake check` evaluates the
module and refuses profile paths in them. With bento's module imported, `services.stew.bento.enable`
(off by default: bento's builder must be able to fetch the flake) adds stew's parts to `programs.bento.settings`: the `stew` channel to
`%t/stew/box.sock` (`bento.hubSocket`, default `/run/user/1000/stew/box.sock`), the tools
`stew` (`#stew-box`) and `hc` (`#herdr-core`) from `bento.flake` (this flake at its rev), the
skills dir copied (at each boot) to `~/.claude/skills` and `~/.config/opencode/skills`, and `STEW_HUB`.

claude-code is unfree, and it is in the default `agentPackages`: `services.stew.enable`
fails evaluation unless your pkgs allow it (`nixpkgs.config.allowUnfree = true`, or an
`allowUnfreePredicate` that accepts `claude-code`). Without unfree, set `agentPackages`
yourself and leave it out.

Config: `~/.config/stew/config.yaml` (`artifacts: <remote>`, `transcripts: <remote>`, `skills: <remote>`, `scan: <dir>|false`, `agent:`,
`model:`, `account:`, `runtime: hc|none`, ...),
the catalog in `~/.config/stew/repos.yaml` (nested groups; a string is a remote).
State: `~/.local/state/stew/state.json`; the artifacts mirror:
`~/.local/share/stew/artifacts.git`; the transcripts checkout:
`~/.local/share/stew/transcripts` (branch `main`); the socket: `$XDG_RUNTIME_DIR/stew/stew.sock`
(`STEW_SOCKET` overrides it for clients).
