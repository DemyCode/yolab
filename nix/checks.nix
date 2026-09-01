# Every check the project has, as nix derivations, so CI runs nothing
# GitHub-specific and "passes locally, red in CI" stops being possible.
#
#   nix run .#ci                                      # everything
#   nix build .#checks.x86_64-linux.local-api-tests   # just one
#
# nix builds from the git index, so a brand new file is invisible until
# `git add -N`. Prefer the bare `.` ref: `path:.` copies the entire working
# directory into the store, which here is 8.6G against 4.8M, on every
# invocation. It is only needed for `#yolab`, whose config.toml is gitignored.
{
  pkgs,
  treefmtEval,
  rust,
  nixosSystems,
}: let
  builds = import ../homelab/builds.nix {inherit pkgs rust;};
  inherit (rust) crates;

  # target/ and node_modules are named as well as gitignored: the identical
  # filter excluded target/ against the working directory but not against the
  # flake source under `path:.`, and the failure mode was 7.5G copied into the
  # store once per check until the disk filled.
  treeSrc =
    pkgs.nix-gitignore.gitignoreRecursiveSource [
      ".git/"
      "target/"
      "node_modules/"
      "result"
      "result-*"
    ]
    ../.;

  toplevel = name: nixosSystems.${name}.config.system.build.toplevel;
in {
  # The shipped bundle itself, not a re-implementation of its build, so the
  # check and the deployed artifact cannot drift. `tsc --noEmit` is not a
  # substitute: it reads tsconfig.app.json alone, while `npm run build` runs
  # `tsc -b` across every project in the solution.
  client-ui = builds.clientUi;

  local-api-tests = crates.local-api.tests;
  installer-tests = crates.installer.tests;

  clippy-local-api = crates.local-api.clippy;
  clippy-installer = crates.installer.clippy;

  # busybox sh, not bash-in-POSIX-mode, because busybox sh is what the Alpine
  # image actually runs this under.
  wg-register-tests =
    pkgs.runCommand "wg-register-tests" {
      nativeBuildInputs = [pkgs.busybox pkgs.jq];
      src = ../apps/wg-register;
    } ''
      cp -r "$src" ./wg-register
      chmod -R +w ./wg-register
      busybox sh ./wg-register/setup_test.sh
      touch $out
    '';

  # `helm lint` accepts charts that cannot run — three shipped that way. This
  # renders each against the yolab-common in this tree, so a library change is
  # caught before release, and asserts the result can actually start.
  charts =
    pkgs.runCommand "chart-checks" {
      nativeBuildInputs = [
        pkgs.kubernetes-helm
        (pkgs.python3.withPackages (ps: [ps.pyyaml]))
      ];
      src = ../apps/catalog;
    } ''
      cp -r "$src" ./catalog
      chmod -R +w ./catalog
      # helm needs a writable home, and the sandbox has none.
      export HOME=$PWD/home
      mkdir -p "$HOME"
      python3 ./catalog/check_charts.py
      touch $out
    '';

  # `coverage-` prefixed so flake.nix keeps these out of `checks`: a coverage
  # percentage CI can fail on invites tests that move the number rather than
  # tests that catch bugs.
  coverage-local-api = crates.local-api.coverage;
  coverage-installer = crates.installer.coverage;

  # Creating a cluster and joining one are different code paths in k3s and in
  # Ceph. Built from the committed CI stubs, so no node's real config.toml is
  # ever touched to run them.
  nixos-create = toplevel "yolab-ci";
  nixos-join = toplevel "yolab-ci-join";
  nixos-wsl = toplevel "yolab-wsl";

  # Same treefmt module `nix fmt` uses, so a file this rejects is a file
  # `nix fmt` fixes.
  formatting = treefmtEval.config.build.check treeSrc;

  # No `-s sh`: forcing one dialect made installer/macos/install.sh fail as
  # The store unit stops k3s for the handover and must start it again without
  # blocking. k3s.service is After= that unit, so a blocking `systemctl start`
  # from inside it deadlocks: the unit waits for k3s's job, k3s's job waits for
  # the unit to finish, and the node sits with k3s dead until the 900s timeout.
  # That happened on node1. Pinned here because it is a property of the
  # generated script, invisible to any Rust or shell test.
  containerd-store-no-block = pkgs.runCommand "containerd-store-no-block" {} ''
    script=${
      pkgs.writeText "store.sh"
      nixosSystems.yolab-ci.config.systemd.services.yolab-containerd-store.script
    }
    grep -q 'systemctl start --no-block k3s.service' "$script" || {
      echo "the store unit must start k3s with --no-block, or it deadlocks against its own After= ordering" >&2
      exit 1
    }
    ! grep -qE 'systemctl start k3s\.service' "$script" || {
      echo "found a blocking 'systemctl start k3s.service' in the store unit" >&2
      exit 1
    }
    touch $out
  '';

  # The image RBD's sizing arithmetic, driven with stubbed `ceph` output.
  #
  # It decides how much of the cluster one node's container store may claim,
  # and the failure it guards against is not local: oversize the image and the
  # pool reaches full-ratio, at which point Ceph blocks writes for every app on
  # every machine. The pool follows the replica policy now, so a logical MB
  # costs `size` raw MB — the case worth pinning is that raising the replica
  # count does NOT raise what the image consumes.
  images-sizing =
    pkgs.runCommand "images-sizing" {
      nativeBuildInputs = [pkgs.jq pkgs.gawk pkgs.bash];
    } ''
      cat > sizing.sh <<'SNIPPET'
      ${
        import ../homelab/nixos/ceph/images-sizing.nix {
          poolName = "images";
          shareOfPool = 0.25;
          minSizeGb = 40;
        }
      }
      SNIPPET

      # RAW MB, replica count, host count -> the WANT_MB the snippet computes.
      want() {
        RAW=$1 REP=$2 HOSTS_N=$3 bash -c '
          timeout() { shift; "$@"; }
          ceph() {
            case "$*" in
              *"osd tree"*)
                printf "{\"nodes\":["
                for i in $(seq 1 "$HOSTS_N"); do
                  [ "$i" -gt 1 ] && printf ","
                  printf "{\"type\":\"host\",\"children\":[1]}"
                done
                printf "]}" ;;
              *"df -f json"*) printf "{\"stats\":{\"total_bytes\":%s}}" "$((RAW * 1048576))" ;;
              *"pool get"*)   printf "{\"size\":%s}" "$REP" ;;
            esac
          }
          . ./sizing.sh >/dev/null 2>&1
          echo "$WANT_MB"
        '
      }

      one=$(want 1000000 1 1)
      two=$(want 1000000 2 1)
      [ "$one" = 250000 ] || { echo "one copy: want 250000, got $one" >&2; exit 1; }
      [ "$two" = 125000 ] || { echo "two copies: want 125000, got $two" >&2; exit 1; }

      # The whole point: two copies of half the image is the same raw bytes.
      [ "$((one * 1))" = "$((two * 2))" ] || {
        echo "raw cost changed with the replica count: $((one * 1)) vs $((two * 2))" >&2
        exit 1
      }

      # The floor never wins over the ceiling — exceeding it is the full-ratio
      # failure, and a too-small image only costs re-pulling layers.
      tiny=$(want 1000 2 4)
      [ "$tiny" -le $((1000 / 2 / (4 * 2))) ] || {
        echo "ceiling lost to the 40G floor on a small pool: got $tiny" >&2
        exit 1
      }

      touch $out
    '';

  # POSIX for using the bash its own shebang asks for. shellcheck reads the
  # shebang. -x follows sourced files.
  shellcheck =
    pkgs.runCommand "shellcheck" {
      nativeBuildInputs = [pkgs.shellcheck];
    } ''
      find ${treeSrc} -name '*.sh' -print0 | xargs -0 shellcheck -x
      touch $out
    '';

  # DL3018 wants every apk package pinned. These images track upstream Alpine
  # deliberately, and the wg-* tools must match the host kernel's WireGuard, so
  # pinning buys a stale userland rather than safety.
  hadolint =
    pkgs.runCommand "hadolint" {
      nativeBuildInputs = [pkgs.hadolint];
    } ''
      find ${treeSrc} -name 'Dockerfile' -print0 \
        | xargs -0 hadolint --ignore DL3018
      touch $out
    '';

  # statix is deliberately absent: its 39 findings are all "avoid repeated keys
  # in attribute sets", and flattening `boot.loader.grub.*` is not obviously an
  # improvement. `nix run nixpkgs#statix -- check` if you want it.
  deadnix =
    pkgs.runCommand "deadnix" {
      nativeBuildInputs = [pkgs.deadnix];
    } ''
      deadnix --fail ${treeSrc}
      touch $out
    '';
}
