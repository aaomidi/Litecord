{
  description = "Litecord - lightweight native Discord client in Rust + Slint";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachSystem [ "x86_64-linux" "aarch64-linux" ] (system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
        lib = pkgs.lib;
        cargoToml = lib.importTOML ./Cargo.toml;

        nativeBuildInputs = with pkgs; [
          pkg-config
          cmake
          nasm
          perl
          git
          makeWrapper
          rustPlatform.bindgenHook
        ];

        buildInputs = with pkgs; [
          alsa-lib
          openssl
          aws-lc # nixpkgs' cargo hook forces aws-lc-sys (str0m DTLS) to use a system copy
          gtk3
          libayatana-appindicator
          xdotool
          libx11
          libxcb
          libxkbcommon
          wayland
          fontconfig
          freetype
          dbus
          gst_all_1.gstreamer
          gst_all_1.gst-plugins-base
        ];

        # Loaded with dlopen at runtime (winit, Slint renderers, tray icon), so they
        # never show up in the binary's NEEDED entries.
        runtimeLibs = with pkgs; [
          libayatana-appindicator
          libxkbcommon
          wayland
          libGL
          vulkan-loader
          libx11
          libxcursor
          libxi
          libxrandr
          fontconfig
          # NVENC path (src/encoder/nvenc.rs) dlopens libavcodec and pokes AVCodecContext at
          # fixed offsets that match libavcodec 61 (FFmpeg 7), not the default ffmpeg.
          ffmpeg_7.lib
        ];

        # NixOS driver libs (libnvidia-encode, libcuda) for NVENC detection.
        hostDriverLibs = "/run/opengl-driver/lib";

        # Binaries the app shells out to (src/**: Command::new).
        runtimeBins = with pkgs; [
          pulseaudio # pactl
          systemd # busctl
          gst_all_1.gstreamer # gst-launch-1.0
          ffmpeg
          xdg-utils
        ];

        # bad: the `va` plugin (vah264enc, vah264dec, vapostproc); pipewire: pipewiresrc for capture.
        # gstreamer itself: coreelements (queue, capsfilter), dropped once the system path is set.
        gstPlugins = (with pkgs.gst_all_1; [ gstreamer gst-plugins-base gst-plugins-good gst-plugins-bad ]) ++ [ pkgs.pipewire ];

        litecord = pkgs.rustPlatform.buildRustPackage {
          pname = "litecord";
          version = cargoToml.package.version;

          src = lib.fileset.toSource {
            root = ./.;
            fileset = lib.fileset.unions [
              ./Cargo.toml
              ./Cargo.lock
              ./build.rs
              ./src
              ./ui
              ./assets
              ./vendor
            ];
          };

          cargoLock.lockFile = ./Cargo.lock;

          inherit nativeBuildInputs buildInputs;

          cargoBuildFlags = [ "--bin" "litecord" ];
          doCheck = false;

          postInstall = ''
            install -Dm644 assets/litecord.desktop -t $out/share/applications
            install -Dm644 assets/litecord.png -t $out/share/icons/hicolor/256x256/apps
          '';

          postFixup = ''
            wrapProgram $out/bin/litecord \
              --prefix LD_LIBRARY_PATH : ${lib.makeLibraryPath runtimeLibs} \
              --suffix LD_LIBRARY_PATH : ${hostDriverLibs} \
              --prefix PATH : ${lib.makeBinPath runtimeBins} \
              --prefix GST_PLUGIN_SYSTEM_PATH_1_0 : ${lib.makeSearchPathOutput "lib" "lib/gstreamer-1.0" gstPlugins}
          '';

          meta = {
            description = cargoToml.package.description;
            homepage = "https://github.com/Ak4ai/Litecord";
            license = lib.licenses.mit;
            mainProgram = "litecord";
            platforms = lib.platforms.linux;
          };
        };
      in
      {
        packages.default = litecord;
        packages.litecord = litecord;

        apps.default = flake-utils.lib.mkApp { drv = litecord; };

        devShells.default = pkgs.mkShell {
          inputsFrom = [ litecord ];
          packages = with pkgs; [ cargo rustc rustfmt clippy rust-analyzer mold ] ++ runtimeBins;
          RUSTFLAGS = "-C link-arg=-fuse-ld=mold";
          LD_LIBRARY_PATH = "${lib.makeLibraryPath runtimeLibs}:${hostDriverLibs}";
          GST_PLUGIN_SYSTEM_PATH_1_0 = lib.makeSearchPathOutput "lib" "lib/gstreamer-1.0" gstPlugins;
        };
      });
}
