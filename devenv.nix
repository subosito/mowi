{ pkgs, lib, ... }:
let
  inherit (pkgs) fetchurl stdenvNoCC;
  inherit (pkgs.stdenv.hostPlatform) system;

  # Microsoft TUI testing CLI (https://github.com/microsoft/tui-test).
  version = "0.1.0-beta.1";
  targets = {
    x86_64-linux = "x86_64-unknown-linux-musl";
    aarch64-linux = "aarch64-unknown-linux-musl";
    x86_64-darwin = "x86_64-apple-darwin";
    aarch64-darwin = "aarch64-apple-darwin";
  };
  hashes = {
    x86_64-linux = "0nbwv4gzyjd6r3c729dacz1ka3prv8dr446pfk9bmdihrsi8rcc2";
    aarch64-linux = "15i8zxf92hjxj5jxkrc08r1jrz8zsqjsbapr30ghf9nqaw5l59zh";
    x86_64-darwin = "0czggbn05c7nq27ighjlr72wxz24447n16p0qn711q4ijqg153p4";
    aarch64-darwin = "1bjzdkwhs3k70ybk4hrxr8zn33wr2fgla73gghqnwjb9rrw2ff3m";
  };

  tui-test = stdenvNoCC.mkDerivation {
    pname = "tui-test";
    inherit version;
    src = fetchurl {
      url = "https://github.com/microsoft/tui-test/releases/download/${version}/tui-test-${targets.${system}}.tar.gz";
      sha256 = hashes.${system} or (throw "tui-test: unsupported system ${system}");
    };
    sourceRoot = ".";
    dontConfigure = true;
    dontBuild = true;
    installPhase = ''
      runHook preInstall
      mkdir -p $out/bin
      install -m755 tui-test $out/bin/tui-test
      runHook postInstall
    '';
    meta = {
      description = "Microsoft TUI testing CLI";
      homepage = "https://github.com/microsoft/tui-test";
      license = lib.licenses.mit;
      mainProgram = "tui-test";
      platforms = lib.attrNames targets;
    };
  };
in
{
  languages.rust.enable = true;
  packages = [ tui-test ];
}
