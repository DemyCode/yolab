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
  # k3s.service is After= the store unit, which is what makes a *blocking*
  # `systemctl start k3s.service` from inside that unit fatal: k3s's start job
  # cannot run until the store unit finishes, so a blocking start would
  # deadlock the node until the unit's own timeout fired. The store unit's
  # body — including that it restarts k3s with --no-block after stopping it —
  # moved to homelab/local-api/src/storage/containerd_store.rs, whose
  # `stops_and_restarts_k3s_around_an_active_migration` test asserts the
  # ordering directly; what is left to assert here is the Nix-level half:
  # that the ordering this property depends on is still in place.
  containerd-store-after-order = pkgs.runCommand "containerd-store-after-order" {} ''
    grep -qx 'yolab-containerd-store.service' ${
      pkgs.writeText "k3s-after"
      (builtins.concatStringsSep "\n" nixosSystems.yolab-ci.config.systemd.services.k3s.after)
    } || {
      echo "k3s.service is no longer After= the store unit — re-read why this check exists" >&2
      exit 1
    }
    touch $out
  '';

  # The image RBD's sizing arithmetic used to be pinned here against a shell
  # fragment driven with stubbed `ceph` output. That fragment moved into
  # homelab/local-api/src/storage/images_sizing.rs (part of the Ceph
  # shell->Rust migration), and `local-api-tests` above already builds and
  # runs its unit tests — including the exact three cases this check used to
  # assert (one/two copies costing the same raw bytes, and the ceiling
  # beating the 40G floor on a small pool) — so a separate nix check would
  # only be testing the same arithmetic twice.

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
