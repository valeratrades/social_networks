{
  inputs = {
    v_flakes.url = "github:valeratrades/v_flakes?ref=v1.6";
    claude_code_nix.url = "github:sadjow/claude-code-nix"; # a newer model needs a newer CLI, and nixpkgs trails the releases
    browser_manipulation = {
      url = "github:valeratrades/browser_manipulation?ref=v0.3.0"; # the same tag as the cargo dep: its driver is pinned to it
      inputs.v_flakes.follows = "v_flakes";
    };
  };
  outputs = { self, v_flakes, claude_code_nix, browser_manipulation }:
    let
      inherit (v_flakes) flake-utils pre-commit-hooks;
    in
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import v_flakes.default_nixpkgs { inherit system; };
        rust = v_flakes.rs.default_nightly system;
        pre-commit-check = pre-commit-hooks.lib.${system}.run (v_flakes.files.preCommit { inherit pkgs; });
        manifest = (pkgs.lib.importTOML ./social_networks/Cargo.toml).package;
        pname = manifest.name;
        stdenv = pkgs.stdenvAdapters.useMoldLinker pkgs.stdenv;
        patchright = browser_manipulation.packages.${system}.patchright;
        driverEnv = {
          PLAYWRIGHT_CLI_JS = "${patchright}/package/cli.js";
          PLAYWRIGHT_NODE_EXE = "${pkgs.nodejs}/bin/node";
        };

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
          jobs.errors.install.packages = [ "mold" ]; # the purpose tests evaluate person files with `nix eval`
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
          PLAYWRIGHT_SKIP_DRIVER_DOWNLOAD = "1";
          # HOME: the rolodex history tests write under ~/.cache. nix: the purpose tests evaluate
          # person files, and a sandbox has no daemon, so the evaluator gets a store it never writes to.
          nativeCheckInputs = [ pkgs.nix ];
          preCheck = ''
            export HOME=$TMPDIR NIX_STATE_DIR=$TMPDIR/nix
            export NIX_CONFIG=$'experimental-features = nix-command\nstore = dummy://'
          '';

          cargoLock = {
            lockFile = ./Cargo.lock;
            outputHashes."browser_manipulation-0.3.0" = "sha256-FCTGZIbAXeq26EuHwa8Rw6yfOKF4NUFdvQ5rSmpoD/4=";
          };
          src = pkgs.lib.cleanSource ./.;
        };
        rustPlatform =
          let build_rust = v_flakes.rs.build_nightly system; in
          pkgs.makeRustPlatform {
            rustc = build_rust;
            cargo = build_rust;
            inherit stdenv;
          };
        # The container is the isolation; chromium's own sandbox needs user
        # namespaces a pod does not get.
        # Skia aborts the browser when fontconfig finds no fonts, and the image has none of its own.
        chromium = pkgs.writeShellScriptBin "chromium" ''
          export FONTCONFIG_FILE=${pkgs.makeFontsConf { fontDirectories = [ pkgs.dejavu_fonts ]; }}
          export XDG_DATA_HOME=/tmp/chromium-xdg-data # NSS writes key4.db/cert9.db under here; keep it off the PVC litestream scans
          mkdir -p "$XDG_DATA_HOME"
          exec ${pkgs.chromium}/bin/chromium --no-sandbox "$@"
        '';
        # One image, every daemon (`contract.daemons` below).
        containerStd = v_flakes.container.implement {
          inherit pkgs pname;
          containers."" = {
            port = null;
            healthPath = null;
            criticality = "normal";
            entrypoint = [ "${bin}/bin/${pname}" ];
            contents = [ chromium claude_code_nix.packages.${system}.default pkgs.coreutils pkgs.nodejs patchright ];
            mounts = [ "/data" ];
            sqlite = [ "/data/.local/state/social_networks/db.sqlite3" ]; # xdg state dir under imageEnv's HOME
            workingDir = "/data";
            imageEnv = [ "HOME=/data" "PATH=/bin" ] ++ pkgs.lib.mapAttrsToList (k: v: "${k}=${v}") driverEnv;
          };
        };
      in
      {
        apps.publish = { type = "app"; program = "${runPublish}/bin/publish"; };
        apps.help = {
          type = "app";
          program = "${pkgs.writeShellScriptBin "help" ''
            cat <<EOF
            nix run .#publish -- major|minor|patch   release the bin crate (see flake.nix)
            nix build .#default                      the ${pname} binary; \`${pname} --help\` lists its commands
            nix develop                              dev shell; regenerates CI workflows and README
            EOF
          ''}/bin/help";
        };

        packages = { default = bin; } // containerStd.packages;

        # Every subcommand that runs forever. Which of them are deployed is the attacher's call.
        containers = pkgs.lib.recursiveUpdate containerStd.containers {
          ${pname}.contract.daemons = [ "dms" "email" "telegram-channel-watch" "twitter" "twitter-schedule" "youtube" ];
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
              PLAYWRIGHT_SKIP_DRIVER_DOWNLOAD = "1";
            } // driverEnv;
          };
      }
    );
}
