# services.stew on home-manager: stew (the host CLI, the in-box client in libexec), hc, and
# the hub's units (./units.nix): the socket-activated daemon, the always-on hc core, and the
# hourly timer (sessions export to the transcripts repo, the skills dir). The
# packages default to this flake's and nixpkgs'; the units use their store paths, never a
# profile, so what host stews' agents may run is `agentPackages` (and `extraPackages`).
# `hostSkills.enable` (off by default) links the stew skill and hc's stub, from the package,
# into the host's claude and opencode skills dirs.
# With bento's module (`programs.bento`) enabled, `bento.enable` adds stew's parts to bento's
# global config: the `stew` channel to the stews' socket, the in-box client and hc as box
# tools (pinned to this flake's rev), the skills dir in both agents' skills dirs, and
# `STEW_HUB`. No `HC_SESSION`: every hc, in boxes and at the hub, uses hc's default session.
self: systems:
{
  config,
  options,
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
    enable = lib.mkEnableOption "stew's hub: the socket-activated daemon, the hc core and the hourly timer";
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
    agentPackages = lib.mkOption {
      type = lib.types.listOf lib.types.package;
      default = with pkgs; [
        claude-code
        opencode
        nix
        gawk
        diffutils
        patch
        gnutar
        gzip
        xz
        unzip
        ripgrep
        fd
        jq
        curl
        less
        which
        file
        procps
        gnumake
      ];
      defaultText = lib.literalExpression "with pkgs; [ claude-code opencode nix gawk diffutils patch gnutar gzip xz unzip ripgrep fd jq curl less which file procps gnumake ]";
      description = ''
        What agents in host stews have on PATH, besides stew's own (stew, hc, git, ssh, sh,
        coreutils, find, grep, sed): the hub's core starts them with its unit's PATH, which
        is built from store paths, never a profile. claude and opencode (claude-code is
        unfree), nix (a project's own tools come from its `nix develop`), and the usual
        command-line tools. The daemon and the hourly export find opencode here too.
        Agents in boxes get their tools from bento's config instead.
      '';
    };
    extraPackages = lib.mkOption {
      type = lib.types.listOf lib.types.package;
      default = lib.optional (config.programs.bento.enable or false) config.programs.bento.package;
      defaultText = lib.literalExpression "lib.optional config.programs.bento.enable config.programs.bento.package";
      example = lib.literalExpression "[ inputs.bento.packages.\${system}.bento pkgs.gh ]";
      description = ''
        More on the units' PATH, after `agentPackages`: bento for the bento driver (by
        default bento's own `programs.bento.package` when that is enabled), and a git
        helper that the git config names without a path (home-manager's `programs.git`
        and `programs.gh` write theirs by store path; one git can't find fails the clone
        and the pushed check, so every remove is refused).
      '';
    };
    hostSkills.enable = lib.mkEnableOption ''
      the stew skill and hc's stub for the host's own agents: linked into
      `~/.claude/skills/` and `~/.config/opencode/skills/` from the stew package
      (`share/stew/skills`), store paths only. Independent of `enable`'';
    bento = {
      enable = lib.mkOption {
        type = lib.types.bool;
        default = config.programs.bento.enable or false;
        defaultText = lib.literalExpression "config.programs.bento.enable";
        description = ''
          stew's parts of bento's global config (`programs.bento.settings`): the `stew`
          channel, the in-box client and hc as tools, the skills dir, `STEW_HUB`.
        '';
      };
      flake = lib.mkOption {
        type = lib.types.str;
        default = "github:GeorgSchenzel/stew/${self.rev or "master"}";
        defaultText = lib.literalExpression ''"github:GeorgSchenzel/stew/''${stew.rev}"'';
        description = ''
          The flake bento's builder builds the box tools from (`#stew-box`, installed as
          `stew`, and `#herdr-core`, hc), pinned: by default this flake's own rev.
        '';
      };
      hubSocket = lib.mkOption {
        type = lib.types.str;
        default = "/run/user/1000/stew/box.sock";
        description = ''
          The `stew` channel's host socket: `stew.socket`'s second socket, `%t/stew/box.sock`
          (bento's config has no `%t`: the user's runtime dir, by uid).
        '';
      };
    };
    idleExit = lib.mkOption {
      type = lib.types.ints.positive;
      default = 600;
      description = "Seconds without a request after which the daemon exits.";
    };
  };

  config = lib.mkMerge (
    [
      (lib.mkIf cfg.hostSkills.enable (
        let
          skill = n: { source = "${cfg.package}/share/stew/skills/${n}/SKILL.md"; };
          links = dir: {
            "${dir}/stew/SKILL.md" = skill "stew";
            "${dir}/herdr-core/SKILL.md" = skill "herdr-core";
          };
        in
        {
          home.file = links ".claude/skills";
          xdg.configFile = links "opencode/skills";
        }
      ))
      (lib.mkIf cfg.enable {
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
            # the git the user's config is written for (helpers by store path)
            git = config.programs.git.package or pkgs.git;
            packages = cfg.agentPackages ++ cfg.extraPackages;
            inherit (cfg) idleExit;
          }
        );
      })
    ]
    # only with bento's module imported (its options exist)
    ++ lib.optional (options ? programs.bento.settings) (
      lib.mkIf cfg.bento.enable {
        programs.bento.settings =
          let
            skills = "~/.local/share/stew/skills";
          in
          {
            channels.stew.socket = cfg.bento.hubSocket;
            tools = {
              stew = "${cfg.bento.flake}#stew-box";
              hc = "${cfg.bento.flake}#herdr-core";
            };
            # one dir (the hub assembles it, `stew skills`), read-only in both agents' dirs
            files = [
              {
                guest = "~/.claude/skills";
                host = skills;
              }
              {
                guest = "~/.config/opencode/skills";
                host = skills;
              }
            ];
            env.STEW_HUB = "unix:/run/bento/stew.sock";
          };
      }
    )
  );
}
