# herdr-core (hc), the agent runtime: the static release build (linux builds are
# static-pie, nothing to patch). Hashes from releases.herdr.dev/core/<version>/manifest.json,
# the same as bento's tools flake (bento@exp/agent tools/flake.nix).
{
  lib,
  stdenvNoCC,
  fetchurl,
}:
let
  version = "0.1.21";
  assets = {
    x86_64-linux = {
      cpu = "x86_64";
      hash = "sha256-eEJHVcEKQN3jigvbVcIKwZcG6a4F7Wj8Ks8fz/DZQcE=";
    };
    aarch64-linux = {
      cpu = "aarch64";
      hash = "sha256-6ESiv6tIoiYJdnnQdryDBC/lf8zvU4fvgC5bJOeLUpo=";
    };
  };
  asset =
    assets.${stdenvNoCC.hostPlatform.system}
      or (throw "herdr-core: no release build for ${stdenvNoCC.hostPlatform.system}");
in
stdenvNoCC.mkDerivation {
  pname = "herdr-core";
  inherit version;
  src = fetchurl {
    url = "https://releases.herdr.dev/core/${version}/herdr-core-linux-${asset.cpu}.tar.gz";
    inherit (asset) hash;
  };
  sourceRoot = ".";
  dontConfigure = true;
  dontBuild = true;
  dontFixup = true;
  installPhase = ''
    runHook preInstall
    install -Dm 0755 herdr-core $out/bin/herdr-core
    ln -s herdr-core $out/bin/hc
    install -Dm 0644 LICENSE THIRD-PARTY-NOTICES -t $out/share/doc/herdr-core/
    runHook postInstall
  '';
  meta = {
    description = "herdr-core (hc): programs and coding agents in terminals that stay alive";
    mainProgram = "hc";
    platforms = builtins.attrNames assets;
  };
}
