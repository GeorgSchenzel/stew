{
  description = "stew — stews of repo clones in a box";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "aarch64-darwin"
        "x86_64-darwin"
        "aarch64-linux"
        "x86_64-linux"
      ];
      forAll = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
      # where the hub runs: hc ships linux builds only
      hubSystems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
    in
    {
      packages = forAll (pkgs: rec {
        # the hub's daemon and the host CLI
        stew = pkgs.callPackage ./nix/package.nix { };
        # the in-box client, installed as `stew` (a bento tool)
        stew-box = pkgs.callPackage ./nix/package.nix { box = true; };
        default = stew;
      } // nixpkgs.lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux {
        # the agent runtime (the hub's core, `stew-hc.service`, and every box's)
        herdr-core = pkgs.callPackage ./nix/herdr-core.nix { };
      });

      # `services.stew` (nix/home-manager.nix): the hub on linux (hc's release builds)
      homeManagerModules.stew = import ./nix/home-manager.nix self hubSystems;
      homeManagerModules.default = self.homeManagerModules.stew;

      # the units as they are generated: store paths only, never a profile; the agents'
      # tools and bento (bento's module enabled) on their PATH
      checks = nixpkgs.lib.genAttrs hubSystems (
        system:
        let
          lib = nixpkgs.lib;
          pkgs = import nixpkgs {
            inherit system;
            config.allowUnfreePredicate = p: lib.getName p == "claude-code";
          };
          stand-in-bento = pkgs.hello;
          # the module itself, evaluated with stand-ins for home-manager's options
          module = lib.evalModules {
            modules = [
              self.homeManagerModules.default
              {
                options = with lib; {
                  assertions = mkOption { type = types.listOf types.attrs; };
                  home.packages = mkOption { type = types.listOf types.package; };
                  systemd.user = mkOption { type = types.attrsOf types.anything; };
                  programs.bento.enable = mkOption { type = types.bool; };
                  programs.bento.package = mkOption { type = types.package; };
                };
                config.services.stew.enable = true;
                config.programs.bento = {
                  enable = true;
                  package = stand-in-bento;
                };
                config._module.args.pkgs = pkgs;
              }
            ];
          };
          units = module.config.systemd.user;
          # without string context: the check doesn't build what the units name
          plain = builtins.unsafeDiscardStringContext (builtins.toJSON units);
          has = p: lib.hasInfix (builtins.unsafeDiscardStringContext p) plain;
          bad = builtins.filter has [
            ".nix-profile"
            "/etc/profiles"
            "/run/current-system"
            "%h/"
          ];
          path = builtins.unsafeDiscardStringContext (
            lib.head (builtins.filter (lib.hasPrefix "PATH=") units.services.stew-hc.Service.Environment)
          );
          onPath = p: lib.hasInfix (builtins.unsafeDiscardStringContext "${p}/bin") path;
        in
        {
          stew = self.packages.${system}.stew;
          units =
            assert lib.assertMsg (bad == [ ]) "the units name a profile: ${toString bad}";
            assert builtins.all (a: a.assertion) module.config.assertions;
            assert builtins.length module.config.home.packages == 2;
            assert has "${self.packages.${system}.stew}/bin/stew daemon";
            assert has "${self.packages.${system}.herdr-core}/bin/herdr-core serve";
            assert has "${self.packages.${system}.stew}/bin/stew export --all";
            assert has "${self.packages.${system}.stew}/bin/stew skills";
            assert units.timers.stew-hourly.Timer.OnCalendar == "hourly";
            assert units.timers.stew-hourly.Install.WantedBy == [ "timers.target" ];
            assert lib.assertMsg (builtins.all onPath [
              pkgs.claude-code
              pkgs.opencode
              pkgs.nix
              pkgs.gnugrep
              stand-in-bento
            ]) "the agents' tools or bento are not on the units' PATH";
            pkgs.writeText "stew-units.json" plain;
        }
      );

      devShells = forAll (pkgs: {
        default = pkgs.mkShell {
          packages = with pkgs; [
            cargo
            rustc
            clippy
            rustfmt
            git
            just
          ]
          # hc, for the runtime's tests
          ++ lib.optional stdenv.hostPlatform.isLinux (pkgs.callPackage ./nix/herdr-core.nix { });
        };
      });
    };
}
