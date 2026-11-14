# home-manager: `services.stew.enable = true;` installs stew and its socket-activated hub
# daemon (the units in ../systemd/). The flake's `homeManagerModules.default` sets the
# package.
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
      description = "The stew package (host CLI and daemon).";
    };
    idleExit = lib.mkOption {
      type = lib.types.ints.positive;
      default = 600;
      description = "Seconds without a request after which the daemon exits.";
    };
  };

  config = lib.mkIf cfg.enable {
    home.packages = [ cfg.package ];

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
