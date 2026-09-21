{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/release-25.11";
    utils.url = "github:numtide/flake-utils";
    naersk.url = "github:nmattia/naersk";
    naersk.inputs.nixpkgs.follows = "nixpkgs";
    rust-overlay.url = "github:oxalica/rust-overlay";
    rust-overlay.inputs.nixpkgs.follows = "nixpkgs";
  };

  outputs =
    {
      self,
      nixpkgs,
      utils,
      naersk,
      rust-overlay,
    }:
    utils.lib.eachDefaultSystem (
      system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs { inherit system overlays; };
        rust = pkgs.rust-bin.stable."1.93.1".default.override {
          targets = [ "x86_64-unknown-linux-musl" ];
          extensions = [
            "llvm-tools-preview"
            "rust-analyzer"
          ];
        };

        naersk-lib = naersk.lib."${system}".override {
          cargo = rust;
          rustc = rust;
        };

        mypython = pkgs.python3.withPackages (
          ps: with ps; [
          ]
        );
      in
      rec {
        packages.xin =
          (naersk-lib.buildPackage {
            pname = "xin";
            root = ./xin;
            nativeBuildInputs = with pkgs; [ pkg-config ];
            buildInputs = with pkgs; [ hdf5 ];
            release = true;
            CARGO_PROFILE_RELEASE_debug = "0";
          }).overrideAttrs
            {
              # postInstall = ''
              #   install -Dm644 ${./xin/completions/xin.fish} \
              #     $out/share/fish/vendor_completions.d/xin.fish
              # '';
            };

        packages.xin_other_linux =
          (naersk-lib.buildPackage {
            pname = "xin";
            root = ./xin;
            nativeBuildInputs = with pkgs; [
              pkg-config
              patchelf
            ];
            buildInputs = with pkgs; [ hdf5 ];
            release = true;
            CARGO_PROFILE_RELEASE_debug = "0";
          }).overrideAttrs
            {
              postInstall = ''
                patchelf $out/bin/xin --set-interpreter "/lib64/ld-linux-x86-64.so.2"
              '';
            };

        packages.xin-docker =
          let
            binary = packages.xin_other_linux;
          in
          pkgs.dockerTools.buildLayeredImage {
            name = "xin";
            tag = "latest";
            contents = [
              pkgs.pkgsStatic.busybox
              pkgs.glibc
              pkgs.hdf5
              binary
            ];
            config = {
              Env = [ "PATH=/usr/local/bin:/bin" ];
              Entrypoint = [ "/bin/xin" ];
              WorkingDir = "/work";
            };
          };

        packages.check = naersk-lib.buildPackage {
          src = ./xin;
          mode = "check";
          name = "xin";
          nativeBuildInputs = with pkgs; [ pkg-config ];
          buildInputs = with pkgs; [ hdf5 ];
        };

        packages.test = naersk-lib.buildPackage {
          pname = "xin";
          root = ./xin;
          mode = "test";
          nativeBuildInputs = with pkgs; [ pkg-config ];
          buildInputs = with pkgs; [ hdf5 ];
        };

        # Cross-check every xin export against anndata itself.
        # Runs via `nix flake check`.
        checks.pytest =
          pkgs.runCommand "xin-pytest"
            {
              buildInputs = [
                mypython
                packages.xin
              ];
              xin_BIN = "${packages.xin}/bin/xin";
            }
            ''
              export HOME="$TMPDIR"
              ${mypython}/bin/python -m pytest ${./tests} -q -p no:cacheprovider --tb=short
              touch "$out"
            '';

        defaultPackage = packages.xin;

        apps.xin = utils.lib.mkApp { drv = packages.xin; };
        defaultApp = apps.xin;

        devShell = pkgs.mkShell {
          shellHook = ''
            #export RUSTFLAGS="-C link-arg=-fuse-ld=mold"
            export XIN_BOOTSTRAP=${pkgs.pkgsStatic.busybox}/bin/busybox
          '';
          nativeBuildInputs = [
            pkgs.bacon
            pkgs.bubblewrap
            pkgs.cargo-nextest
            #pkgs.mold
            #pkgs.pkg-config
            pkgs.ripgrep
            mypython
            rust
          ];
          # buildInputs = [
          #   # xin's build-sandbox bootstrap: a full static busybox
          #   pkgs.pkgsStatic.busybox
          # ];
        };
      }
    );
}
