# home-manager: `services.stew.enable = true;` installs stew, its socket-activated hub
# daemon and the hub's always-on hc core (the units in ../systemd/). The flake's
# `homeManagerModules.default` sets the packages. (The bento side, stew's channel and the
# in-box client and hc as tools, the skills dir, HC_SESSION and STEW_HUB in the global
# config, comes with bento v2's module: B5.)
{
  config,
  lib,
  ...
}:
let
  cfg = config.services.stew;
in
{
  options.services.stew = {
    enable = lib.mkEnableOption "stew's hub daemon, socket-activated";
    package = lib.mkOption {
      type = lib.types.package;
      description = "The stew package (host CLI and daemon, the in-box client in libexec).";
    };
    hcPackage = lib.mkOption {
      type = lib.types.package;
      description = "herdr-core (hc), the agent runtime: the hub's core and the CLI the daemon calls.";
    };
    idleExit = lib.mkOption {
      type = lib.types.ints.positive;
      default = 600;
      description = "Seconds without a request after which the daemon exits.";
    };
  };

  config = lib.mkIf cfg.enable {
    home.packages = [
      cfg.package
      cfg.hcPackage
    ];

    # the hub's hc core: always on, its own unit (the daemon exits when idle)
    systemd.user.services.stew-hc = {
      Unit = {
        Description = "stew hub hc core (herdr-core, session stew)";
        StartLimitIntervalSec = 0;
      };
      Service = {
        ExecStart = "%h/.nix-profile/bin/herdr-core serve";
        Environment = [
          "HC_SESSION=stew"
          "PATH=%h/.nix-profile/bin:/etc/profiles/per-user/%u/bin:/run/current-system/sw/bin"
          "GIT_TERMINAL_PROMPT=0"
        ];
        Restart = "always";
        RestartSec = 10;
      };
      Install.WantedBy = [ "default.target" ];
    };

    systemd.user.sockets.stew = {
      Unit.Description = "stew hub socket";
      Socket = {
        ListenStream = "%t/stew/stew.sock";
        SocketMode = "0600";
        DirectoryMode = "0700";
      };
      Install.WantedBy = [ "sockets.target" ];
    };

    systemd.user.services.stew = {
      Unit = {
        Description = "stew hub daemon";
        Requires = [ "stew.socket" ];
        After = [ "stew.socket" ];
      };
      Service = {
        # by name from the profile, never a store path
        ExecStart = "%h/.nix-profile/bin/stew daemon --idle-exit ${toString cfg.idleExit}";
        Environment = [
          "PATH=%h/.nix-profile/bin:/etc/profiles/per-user/%u/bin:/run/current-system/sw/bin"
          "GIT_TERMINAL_PROMPT=0"
        ];
      };
    };
  };
}
