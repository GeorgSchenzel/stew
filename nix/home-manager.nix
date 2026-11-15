# services.stew on home-manager: stew (the host CLI, the in-box client in libexec), hc, and
# the hub's units (./units.nix): the socket-activated daemon and the always-on hc core. The
# packages default to this flake's; the units use their store paths, never a profile.
# (The bento side, the `stew` channel, the in-box client and hc as box tools, the skills
# dir, HC_SESSION and STEW_HUB in bento's global config, comes with bento v2's module: B5.)
self: systems:
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.services.stew;
  system = pkgs.stdenv.hostPlatform.system;
  supported = builtins.elem system systems;
in
{
  options.services.stew = {
    enable = lib.mkEnableOption "stew's hub: the socket-activated daemon and the hc core";
    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${system}.stew;
      defaultText = lib.literalExpression "stew.packages.\${system}.stew";
      description = "The stew package (host CLI and daemon, the in-box client in libexec).";
    };
    hcPackage = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${system}.herdr-core;
      defaultText = lib.literalExpression "stew.packages.\${system}.herdr-core";
      description = "herdr-core (hc), the agent runtime: the hub's core and the CLI the daemon calls.";
    };
    extraPackages = lib.mkOption {
      type = lib.types.listOf lib.types.package;
      default = [ ];
      example = lib.literalExpression "[ inputs.bento.packages.\${system}.bento ]";
      description = "More on the units' PATH: bento (the bento driver), and on the host driver the agents (claude, opencode) the hub's core starts in host stews.";
    };
    idleExit = lib.mkOption {
      type = lib.types.ints.positive;
      default = 600;
      description = "Seconds without a request after which the daemon exits.";
    };
  };

  config = lib.mkIf cfg.enable {
    assertions = [
      {
        assertion = supported;
        message = "stew's hub is only packaged for ${lib.concatStringsSep ", " systems} (hc's release builds).";
      }
    ];

    home.packages = lib.mkIf supported [
      cfg.package
      cfg.hcPackage
    ];

    systemd.user = lib.mkIf supported (
      import ./units.nix {
        inherit lib pkgs;
        stew = cfg.package;
        hc = cfg.hcPackage;
        inherit (cfg) extraPackages idleExit;
      }
    );
  };
}
