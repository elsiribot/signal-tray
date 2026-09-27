{
  description = "signal-tray: a tiny, keyboard-driven Signal client that floats in a screen corner";
  # AGPL-3.0-only, inherited from presage.

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    crane.url = "github:ipetkov/crane";
  };

  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
      crane,
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
        craneLib = crane.mkLib pkgs;

        nativeBuildInputs = with pkgs; [
          pkg-config
          wrapGAppsHook4
          makeWrapper
          # libsignal / libsignal-service generate code from .proto files
          protobuf
          cmake
          rustPlatform.bindgenHook
        ];

        buildInputs = with pkgs; [
          gtk4
          gtk4-layer-shell
          glib
          # SQLCipher (bundled via presage-store-sqlite) links libcrypto
          openssl
        ];

        # Only what the build reads, so editing docs or the flake's other
        # outputs doesn't rebuild the package.
        src = pkgs.lib.fileset.toSource {
          root = ./.;
          fileset = pkgs.lib.fileset.unions [
            ./Cargo.toml
            ./Cargo.lock
            ./src
            ./data
          ];
        };

        # crane vendors git dependencies the way `cargo package` would, which
        # drops presage-store-sqlite's `.sqlx` query cache; without it sqlx's
        # macros need a live database to compile.
        cargoVendorDir = craneLib.vendorCargoDeps {
          inherit src;
          overrideVendorGitCheckout =
            packages: drv:
            if pkgs.lib.any (p: p.name == "presage-store-sqlite") packages then
              drv.overrideAttrs (old: {
                postInstall = (old.postInstall or "") + ''
                  cp -r presage-store-sqlite/.sqlx $out/presage-store-sqlite-*/
                '';
              })
            else
              drv;
        };

        commonArgs = {
          # Deps and package must see the same inputs: the GTK -sys crates
          # rerun their build scripts when the pkg-config environment
          # differs, which would rebuild them with the package.
          inherit
            src
            buildInputs
            nativeBuildInputs
            cargoVendorDir
            ;
          strictDeps = true;
        };

        # All dependencies, built once and reused until Cargo.lock or the
        # build inputs change. presage and libsignal are git dependencies
        # pinned by rev in Cargo.lock, which crane fetches on its own.
        cargoArtifacts = craneLib.buildDepsOnly commonArgs;
      in
      {
        packages.default = craneLib.buildPackage (
          commonArgs
          // {
            inherit cargoArtifacts;

            postInstall = ''
              install -Dm644 data/io.github.elsirion.SignalTray.desktop \
                $out/share/applications/io.github.elsirion.SignalTray.desktop
            '';

            meta.mainProgram = "signal-tray";
          }
        );

        devShells.default = pkgs.mkShell {
          inherit buildInputs;
          nativeBuildInputs =
            nativeBuildInputs
            ++ (with pkgs; [
              cargo
              rustc
              clippy
              rustfmt
              rust-analyzer
              just
            ]);
        };
      }
    );
}
