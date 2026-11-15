# The hub's user units, as home-manager's `systemd.user` attrsets: the socket-activated
# daemon (`stew.socket`, `stew.service`), the hub's always-on hc core (`stew-hc.service`),
# and the hourly timer (`stew-hourly.timer`: sessions export, the skills dir).
# Everything comes from store paths (never a profile): ExecStart names the packages, and
# PATH is built from them. Every unit's PATH reaches `stew` (hc's vias run `stew exec` by
# name), hc, git, ssh, sh and the coreutils, find, grep and sed the scripts use, plus
# `extraPackages` (bento for the bento driver; on the host driver, the agents the hub's core
# starts in host stews, and opencode for their sessions export).
{
  lib,
  pkgs,
  stew,
  hc,
  extraPackages ? [ ],
  idleExit ? 600,
}:
let
  path = lib.makeBinPath (
    [
      stew
      hc
      pkgs.git
      pkgs.openssh
      pkgs.bash
      pkgs.coreutils
      pkgs.findutils
      pkgs.gnugrep
      pkgs.gnused
    ]
    ++ extraPackages
  );
  env = [
    "PATH=${path}"
    "GIT_TERMINAL_PROMPT=0"
  ];
in
{
  sockets.stew = {
    Unit.Description = "stew hub socket";
    Socket = {
      ListenStream = "%t/stew/stew.sock";
      SocketMode = "0600";
      DirectoryMode = "0700";
    };
    Install.WantedBy = [ "sockets.target" ];
  };

  services.stew = {
    Unit = {
      Description = "stew hub daemon";
      Requires = [ "stew.socket" ];
      After = [ "stew.socket" ];
    };
    Service = {
      ExecStart = "${lib.getExe stew} daemon --idle-exit ${toString idleExit}";
      Environment = env;
    };
  };

  # the hub's hc core: always on, its own unit, so the daemon can exit when idle while the
  # core and the agents' terminals stay up. `serve` exits 0 at once while another core of
  # the session holds its lock: retry every 10 s, never give up.
  services.stew-hc = {
    Unit = {
      Description = "stew hub hc core (herdr-core, session stew)";
      StartLimitIntervalSec = 0;
    };
    Service = {
      ExecStart = "${hc}/bin/herdr-core serve";
      Environment = env ++ [ "HC_SESSION=stew" ];
      Restart = "always";
      RestartSec = 10;
    };
    Install.WantedBy = [ "default.target" ];
  };

  # hourly: every running stew's sessions into the transcripts repo (committed and pushed
  # daily; `stop` and `remove` export and push themselves), then the skills dir boxes see
  # (the artifacts repo's `main:skills/`). Both are daemon calls (socket-activated); a
  # failed export doesn't skip the skills (`-`).
  services.stew-hourly = {
    Unit = {
      Description = "stew hourly: export sessions to the transcripts repo, assemble the skills dir";
      Wants = [ "stew.socket" ];
      After = [ "stew.socket" ];
    };
    Service = {
      Type = "oneshot";
      ExecStart = [
        "-${lib.getExe stew} export --all"
        "${lib.getExe stew} skills"
      ];
      Environment = env;
    };
  };

  timers.stew-hourly = {
    Unit.Description = "stew hourly: sessions export and skills";
    Timer = {
      OnCalendar = "hourly";
      Persistent = true;
      RandomizedDelaySec = 300;
    };
    Install.WantedBy = [ "timers.target" ];
  };
}
