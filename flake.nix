{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/f61125a668a320878494449750330ca58b78c557";
    rust-overlay.url = "github:oxalica/rust-overlay/7ed7e8c74be95906275805db68201e74e9904f07";
    flake-utils.url = "github:numtide/flake-utils/11707dc2f618dd54ca8739b309ec4fc024de578b";
    pre-commit-hooks.url = "github:cachix/git-hooks.nix/ca5b894d3e3e151ffc1db040b6ce4dcc75d31c37";
    v_flakes.url = "github:valeratrades/v_flakes?ref=v1.6";
  };
  outputs = { self, nixpkgs, rust-overlay, flake-utils, pre-commit-hooks, v_flakes }:
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
        pre-commit-check = pre-commit-hooks.lib.${system}.run (v_flakes.files.preCommit { inherit pkgs; });
        manifest = (pkgs.lib.importTOML ./Cargo.toml).package;
        pname = manifest.name;
        stdenv = pkgs.stdenvAdapters.useMoldLinker pkgs.stdenv;

        rs = v_flakes.rs {
          inherit pkgs rust;
          cranelift = true;
          build = {
            enable = true;
            workspace."./" = [ "git_version" "log_directives" ];
          };
        };
        github = v_flakes.github {
          inherit pkgs pname rs;
          enable = true;
          lastSupportedVersion = "nightly-2025-10-10";
          jobs.default = true;
          jobs.warnings.install = { packages = [ "mold" ]; debug = true; };
          release.default = true;
        };
        readme = v_flakes.readme-fw {
          inherit pkgs pname;
          lastSupportedVersion = "nightly-1.92";
          rootDir = ./.;
          licenses = [{ license = v_flakes.files.licenses.nsfw; }];
          badges = [ "msrv" "crates_io" "docs_rs" "loc" "ci" ];
        };
        combined = v_flakes.utils.combine [ rs github readme ];
      in
      {
        packages =
          let
            rustc = rust;
            cargo = rust;
            rustPlatform = pkgs.makeRustPlatform {
              inherit rustc cargo stdenv;
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
              src = pkgs.lib.cleanSource ./.;
            };
          };

        devShells.default =
          with pkgs;
          mkShell {
            inherit stdenv;
            shellHook =
              pre-commit-check.shellHook +
              combined.shellHook +
              ''
                cp -f ${(v_flakes.files.treefmt) { inherit pkgs; }} ./.treefmt.toml
              '';
            packages = [
              mold-wrapped
              openssl
              pkg-config
              rust
            ] ++ pre-commit-check.enabledPackages ++ combined.enabledPackages;

            env = {
              RUST_BACKTRACE = 1;
              RUST_LIB_BACKTRACE = 0;
              CARGO_PROFILE_DEV_BUILD_OVERRIDE_DEBUG = true;
            };
          };
      }
    );
}
