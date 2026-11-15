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

      # the units as they are generated: store paths only, never a profile
      checks = nixpkgs.lib.genAttrs hubSystems (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
          # the module itself, evaluated with stand-ins for home-manager's options
          module = nixpkgs.lib.evalModules {
            modules = [
              self.homeManagerModules.default
              {
                options = with nixpkgs.lib; {
                  assertions = mkOption { type = types.listOf types.attrs; };
                  home.packages = mkOption { type = types.listOf types.package; };
                  systemd.user = mkOption { type = types.attrsOf types.anything; };
                };
                config.services.stew.enable = true;
                config._module.args.pkgs = pkgs;
              }
            ];
          };
          units = module.config.systemd.user;
          text = builtins.toJSON units;
          plain = builtins.unsafeDiscardStringContext text;
          has = p: nixpkgs.lib.hasInfix (builtins.unsafeDiscardStringContext p) plain;
          bad = builtins.filter has [
            ".nix-profile"
            "/etc/profiles"
            "/run/current-system"
            "%h/"
          ];
        in
        {
          units =
            assert nixpkgs.lib.assertMsg (bad == [ ]) "the units name a profile: ${toString bad}";
            assert builtins.all (a: a.assertion) module.config.assertions;
            assert builtins.length module.config.home.packages == 2;
            assert has "${self.packages.${system}.stew}/bin/stew daemon";
            assert has "${self.packages.${system}.herdr-core}/bin/herdr-core serve";
            pkgs.writeText "stew-units.json" text;
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
