{
  description = "tuyau: declarative PipeWire patchbay with a GPU-rendered graph editor and a CLI";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});

      runtimeLibraries =
        pkgs: with pkgs; [
          libxkbcommon
          vulkan-loader
          wayland
          libx11
          libxcb
          libxcursor
          libxi
          libxrandr
          fontconfig
          freetype
        ];

      buildInputs =
        pkgs:
        with pkgs;
        [
          pipewire
          alsa-lib
          libdrm
          mesa
          openssl
          zlib
        ]
        ++ runtimeLibraries pkgs;

      nativeBuildInputs =
        pkgs: with pkgs; [
          pkg-config
          cmake
          makeWrapper
          rustPlatform.bindgenHook
        ];
    in
    {
      packages = forAllSystems (pkgs: {
        default = self.packages.${pkgs.stdenv.hostPlatform.system}.tuyau;
        tuyau = pkgs.rustPlatform.buildRustPackage {
          pname = "tuyau";
          version = (fromTOML (builtins.readFile ./Cargo.toml)).workspace.package.version;
          src = pkgs.lib.cleanSource ./.;
          cargoLock = {
            lockFile = ./Cargo.lock;
            allowBuiltinFetchGit = true;
          };
          nativeBuildInputs = nativeBuildInputs pkgs;
          buildInputs = buildInputs pkgs;
          postInstall = ''
            install -Dm644 assets/tuyau.desktop $out/share/applications/tuyau.desktop
            install -Dm644 assets/icon.svg $out/share/icons/hicolor/scalable/apps/tuyau.svg
          '';
          postFixup = ''
            wrapProgram $out/bin/tuyau \
              --prefix LD_LIBRARY_PATH : ${pkgs.lib.makeLibraryPath (runtimeLibraries pkgs)}
          '';
          meta = {
            description = "Declarative PipeWire patchbay";
            mainProgram = "tuyau";
            platforms = pkgs.lib.platforms.linux;
          };
        };
      });

      devShells = forAllSystems (
        pkgs:
        let
          # gcc only knows bfd, gold, lld and mold for -fuse-ld; -B makes it pick up wild as `ld`.
          wildAsLd = pkgs.runCommand "wild-as-ld" { } ''
            mkdir -p $out/bin
            ln -s ${pkgs.wild}/bin/wild $out/bin/ld
          '';
        in
        {
          default = pkgs.mkShell {
            packages =
              with pkgs;
              [
                cargo
                rustc
                rustfmt
                clippy
                rust-analyzer
              ]
              ++ nativeBuildInputs pkgs
              ++ buildInputs pkgs;
            LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath (runtimeLibraries pkgs);
            "CARGO_TARGET_${pkgs.stdenv.hostPlatform.rust.cargoEnvVarTarget}_RUSTFLAGS" =
              "-C link-arg=-B${wildAsLd}/bin";
          };
        }
      );

      homeManagerModules.default = import ./nix/home-manager.nix self;

      formatter = forAllSystems (pkgs: pkgs.nixfmt);
    };
}
