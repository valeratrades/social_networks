{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay.url = "github:oxalica/rust-overlay";
    flake-utils.url = "github:numtide/flake-utils";
    pre-commit-hooks.url = "github:cachix/git-hooks.nix/ca5b894d3e3e151ffc1db040b6ce4dcc75d31c37";
    v-utils.url = "github:valeratrades/.github?ref=v1.4";
  };
  outputs = { self, nixpkgs, rust-overlay, flake-utils, pre-commit-hooks, v-utils }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs {
          inherit system overlays;
          allowUnfree = true;
        };
        ##NB: can't load rust-bin from nightly.latest, as there are week guarantees of which components will be available on each day.
        rust = pkgs.rust-bin.selectLatestNightlyWith (toolchain: toolchain.default.override {
          extensions = [ "rust-src" "rust-analyzer" "rust-docs" "rustc-codegen-cranelift-preview" ];
        });
        #rust = pkgs.rust-bin.nightly."2025-10-10".default;
        pre-commit-check = pre-commit-hooks.lib.${system}.run (v-utils.files.preCommit { inherit pkgs; });
        manifest = (pkgs.lib.importTOML ./Cargo.toml).package;
        pname = manifest.name;
        stdenv = pkgs.stdenvAdapters.useMoldLinker pkgs.stdenv;

        alwaysPkgNames = [ "mold" ];
        alwaysPkgs = map (name: pkgs.${name}) alwaysPkgNames ++ [ pkgs.openssl.dev ];

        github =
          let
            jobDeps = { packages = alwaysPkgNames ++ [ "pkg-config" ]; debug = true; };
          in
          v-utils.github {
            inherit pkgs pname;
            langs = [ "rs" ];
            lastSupportedVersion = "nightly-2025-10-10";
            jobs.default = true;
            release.default = true;
            install = jobDeps;
          };
        rs = v-utils.rs {
          inherit pkgs rust;
          cranelift = true;
          build = {
            enable = true;
            workspace."./" = [ "git_version" "log_directives" ];
          };
        };
        readme = v-utils.readme-fw {
          inherit pkgs pname;
          lastSupportedVersion = "nightly-1.92";
          rootDir = ./.;
          licenses = [{ license = v-utils.files.licenses.nsfw; }];
          badges = [ "msrv" "crates_io" "docs_rs" "loc" "ci" ];
        };
        combined = v-utils.utils.combine [ rs github readme ];
      in
      {
        packages =
          let
            rustc = rust;
            cargo = rust;
            rustPlatform = pkgs.makeRustPlatform {
              inherit rustc cargo stdenv;
            };
            # Filter out .cargo/config.toml (dev-only flags: cranelift, mold, etc.)
            cleanSrc = pkgs.lib.cleanSourceWith {
              src = ./.;
              filter = path: type:
                !(pkgs.lib.hasSuffix ".cargo/config.toml" path)
                && !(pkgs.lib.hasSuffix ".cargo/config" path);
            };
          in
          {
            default = rustPlatform.buildRustPackage {
              inherit pname;
              version = manifest.version;

              buildInputs = with pkgs; [
                openssl.dev
              ];
              nativeBuildInputs = with pkgs; [ pkg-config ];

              cargoLock.lockFile = ./Cargo.lock;
              src = cleanSrc;
            };
          } // (if pkgs.stdenv.hostPlatform.isLinux then {
            # Musl-static build for portable Linux binaries (used by release CI)
            static =
              let
                muslTarget = "x86_64-unknown-linux-musl";
                pkgsMusl = pkgs.pkgsCross.musl64;
                opensslStatic = pkgsMusl.pkgsStatic.openssl;
                # Use host (glibc) rustc/cargo with musl target added.
                # pkgsCross.musl64.makeRustPlatform sets hostPlatform correctly for --target,
                # but uses our native glibc toolchain binaries.
                rustStatic = pkgs.rust-bin.selectLatestNightlyWith (toolchain:
                  toolchain.minimal.override {
                    targets = [ muslTarget ];
                  }
                );
                rustPlatformMusl = pkgsMusl.makeRustPlatform {
                  rustc = rustStatic;
                  cargo = rustStatic;
                };
              in
              rustPlatformMusl.buildRustPackage {
                inherit pname;
                version = manifest.version;

                nativeBuildInputs = [ pkgs.pkg-config ];

                env.OPENSSL_STATIC = "1";
                env.OPENSSL_DIR = "${opensslStatic.dev}";
                env.OPENSSL_LIB_DIR = "${opensslStatic.out}/lib";
                env.OPENSSL_INCLUDE_DIR = "${opensslStatic.dev}/include";
                env.RUSTFLAGS = "-C target-feature=+crt-static";

                cargoLock.lockFile = ./Cargo.lock;
                src = cleanSrc;
              };
          } else { });

        devShells.default =
          with pkgs;
          mkShell {
            inherit stdenv;
            shellHook =
              pre-commit-check.shellHook +
              combined.shellHook +
              ''
                cp -f ${(v-utils.files.treefmt) { inherit pkgs; }} ./.treefmt.toml
              '';
            packages =
              alwaysPkgs ++
              [
                mold
                openssl
                pkg-config
                rust
              ] ++ pre-commit-check.enabledPackages ++ combined.enabledPackages;

            env.RUST_BACKTRACE = 1;
            env.RUST_LIB_BACKTRACE = 0;
          };
      }
    );
}
