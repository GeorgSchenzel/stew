# stew's host bin, or with `box = true` the in-box client (renamed to `stew`).
{
  lib,
  rustPlatform,
  makeWrapper,
  gitMinimal,
  box ? false,
}:
rustPlatform.buildRustPackage {
  pname = if box then "stew-box" else "stew";
  version = (builtins.fromTOML (builtins.readFile ../Cargo.toml)).workspace.package.version;

  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      ../Cargo.toml
      ../Cargo.lock
      ../crates
      ../skills
    ];
  };

  cargoLock.lockFile = ../Cargo.lock;
  # the host package carries the in-box client too: host-driver stews get it first on PATH
  cargoBuildFlags =
    if box then
      [
        "-p"
        "stew-box"
      ]
    else
      [
        "-p"
        "stew"
        "-p"
        "stew-box"
      ];

  # integration tests shell out to git; `just test` runs them
  doCheck = false;

  nativeBuildInputs = lib.optional (!box) makeWrapper;

  # the hub runs git itself (mirror, ls-remote); --suffix lets a git on PATH win
  postInstall =
    if box then
      ''
        mv $out/bin/stew-box $out/bin/stew
      ''
    else
      ''
        mkdir -p $out/libexec/stew $out/share/stew
        mv $out/bin/stew-box $out/libexec/stew/stew
        # the skills `stew skills` assembles (stew's and hc's stub), for `hostSkills`
        cp -r ${../skills} $out/share/stew/skills
        wrapProgram $out/bin/stew \
          --suffix PATH : ${lib.makeBinPath [ gitMinimal ]}
      '';

  meta = {
    description = "Stews of repo clones in a box";
    mainProgram = "stew";
    platforms = lib.platforms.unix;
  };
}
