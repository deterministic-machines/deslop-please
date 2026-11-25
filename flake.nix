{
  description = "deslop-please";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay.url = "github:oxalica/rust-overlay";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs =
    {
      nixpkgs,
      rust-overlay,
      flake-utils,
      ...
    }:
    flake-utils.lib.eachSystem [ "x86_64-linux" ] (
      system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ (import rust-overlay) ];
        };
        # The lints read rust-analyzer's index, so its version is part of what
        # they report: pinned with the Rust it comes with.
        rustToolchain = pkgs.rust-bin.stable."1.99.0".minimal.override {
          extensions = [
            "rust-src"
            "rust-analyzer"
            "clippy"
            "rustfmt"
          ];
        };
        # The Java tests index Maven and Gradle projects with scip-java, which
        # nixpkgs does not have: its jars, fetched from Maven Central by
        # coursier, and a launcher on the JDK the projects build with.
        jdk = pkgs.jdk17;
        scipJavaVersion = "0.12.3";
        scipJavaJars = pkgs.stdenvNoCC.mkDerivation {
          pname = "scip-java-jars";
          version = scipJavaVersion;
          dontUnpack = true;
          nativeBuildInputs = [ pkgs.coursier ];
          buildPhase = ''
            export COURSIER_CACHE="$TMPDIR/coursier"
            mkdir -p "$out/share/java"
            cs fetch "com.sourcegraph:scip-java_2.13:${scipJavaVersion}" \
              | xargs -I{} cp {} "$out/share/java/"
          '';
          dontInstall = true;
          outputHashMode = "recursive";
          outputHashAlgo = "sha256";
          outputHash = "sha256-oa4onjhsrHmiPsb12WvLTiwDtmotKvinRePA6H+ORow=";
        };
        scipJava = pkgs.writeShellScriptBin "scip-java" ''
          exec ${jdk}/bin/java -cp "${scipJavaJars}/share/java/*" \
            com.sourcegraph.scip_java.ScipJava "$@"
        '';
      in
      {
        devShells.default = pkgs.mkShell {
          packages = [
            rustToolchain
            jdk
            pkgs.maven
            pkgs.gradle
            scipJava
          ];
        };
      }
    );
}
