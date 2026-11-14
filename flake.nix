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
      });

      devShells = forAll (pkgs: {
        default = pkgs.mkShell {
          packages = with pkgs; [
            cargo
            rustc
            clippy
            rustfmt
            git
            just
          ];
        };
      });
    };
}
