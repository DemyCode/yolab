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

  toplevel  = name: nixosSystems.${name}.config.system.build.toplevel;
in {
  # The shipped bundle itself, not a re-implementation of its build, so the
  # check and the deployed artifact cannot drift. `tsc --noEmit` is not a
  # substitute: it reads tsconfig.app.json alone, while `npm run build` runs
  # `tsc -b` across every project in the solution.
  client-ui = builds.clientUi;

  local-api-tests = crates.local-api.tests;
  installer-tests = crates.installer.tests;

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
