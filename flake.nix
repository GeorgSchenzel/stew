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

      # `services.stew` (nix/home-manager.nix) with this flake's package
      homeManagerModules.default =
        { pkgs, lib, ... }:
        {
          imports = [ ./nix/home-manager.nix ];
          services.stew.package = lib.mkDefault self.packages.${pkgs.stdenv.hostPlatform.system}.stew;
          services.stew.hcPackage = lib.mkDefault self.packages.${pkgs.stdenv.hostPlatform.system}.herdr-core;
        };

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
