{
  inputs = {
    v_flakes.url = "github:valeratrades/v_flakes?ref=v1.6";
  };
  outputs = { self, v_flakes }:
    let
      inherit (v_flakes) flake-utils pre-commit-hooks;
    in
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import v_flakes.default_nixpkgs { inherit system; config.allowUnfree = true; }; # claude-code
        rust = v_flakes.rs.default_nightly system;
        pre-commit-check = pre-commit-hooks.lib.${system}.run (v_flakes.files.preCommit { inherit pkgs; });
        manifest = (pkgs.lib.importTOML ./social_networks/Cargo.toml).package;
        pname = manifest.name;
        stdenv = pkgs.stdenvAdapters.useMoldLinker pkgs.stdenv;

        rs = v_flakes.rs {
          inherit pkgs rust;
          cranelift = true;
          build = {
            enable = true;
            workspace."./social_networks" = [ "git_version" "log_directives" ];
            workspace."./social_networks_adapters" = [ ];
            workspace."./social_networks_utils" = [ ];
          };
        };
        github = v_flakes.github {
          inherit pkgs pname rs;
          enable = true;
          lastSupportedVersion = "nightly-${v_flakes.rs.nightly_version}";
          jobs.default = true;
          jobs.warnings.install = { packages = [ "mold" ]; debug = true; };
          containerRelease = { registry = "ghcr.io/valeratrades"; };
          release = {
            default = true;
            cargoTomlPath = "./social_networks/Cargo.toml";
          };
        };
        readme = v_flakes.readme-fw {
          inherit pkgs pname;
          lastSupportedVersion = "nightly-1.92";
          rootDir = ./.;
          licenses = [{ license = v_flakes.files.licenses.gl; }];
          badges = [ "msrv" "crates_io" "docs_rs" "loc" "ci" ];
        };
        combined = v_flakes.utils.combine { inherit rust; modules = [ rs github readme ]; };

        # `nix run .#publish -- major|minor|patch` → cargo-release: bump the bin
        # crate, commit, tag `v{version}`, push, publish to crates.io. The tag push
        # triggers release-*.yml (attaches per-target binaries) and bumps the
        # crates.io version binstall resolves against — both keyed on `v{version}`.
        # --no-verify: the release CI already does a full cross-platform build.
        runPublish = pkgs.writeShellApplication {
          name = "publish";
          runtimeInputs = [ pkgs.cargo-release pkgs.git rust ];
          text = ''
            part="''${1:-}"
            case "$part" in major|minor|patch) ;; *) echo "usage: nix run .#publish -- major|minor|patch" >&2; exit 1 ;; esac
            exec cargo release "$part" --execute --no-confirm --no-verify
          '';
        };

        bin = rustPlatform.buildRustPackage {
          inherit pname;
          version = manifest.version;

          buildInputs = with pkgs; [
            openssl.dev
          ];
          nativeBuildInputs = with pkgs; [ pkg-config ];
          RUSTC_WRAPPER = ""; # .cargo/config.toml sets sccache, absent in the sandbox
          preCheck = "export HOME=$TMPDIR"; # the rolodex history tests write under ~/.cache

          cargoLock.lockFile = ./Cargo.lock;
          src = pkgs.lib.cleanSource ./.;
        };
        rustPlatform = pkgs.makeRustPlatform {
          rustc = rust;
          cargo = rust;
          inherit stdenv;
        };
        # The container is the isolation; chromium's own sandbox needs user
        # namespaces a pod does not get.
        chromium = pkgs.writeShellScriptBin "chromium" ''exec ${pkgs.chromium}/bin/chromium --no-sandbox "$@"'';
        # One image, every daemon: which of them run, and so which subcommand each
        # pod is given, is the deployment's decision (devops, tenant `personal`).
        containerStd = v_flakes.container.implement {
          inherit pkgs pname;
          containers."" = {
            port = null;
            healthPath = null;
            criticality = "normal";
            entrypoint = [ "${bin}/bin/${pname}" ];
            contents = [ chromium pkgs.claude-code pkgs.coreutils ];
            mounts = [ "/data" ];
            workingDir = "/data";
            imageEnv = [ "HOME=/data" "PATH=/bin" ];
          };
        };
      in
      {
        apps.publish = { type = "app"; program = "${runPublish}/bin/publish"; };

        packages = { default = bin; } // containerStd.packages;

        containers = containerStd.containers;

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
              chromium # skool mints its session cookie by driving one; the systemd unit needs it on PATH too
              mold
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
