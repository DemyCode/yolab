{
  pkgs,
  treefmtEval,
  rust,
  nixosSystems,
}: let
  builds = import ../homelab/builds.nix {inherit pkgs rust;};
  inherit (rust) crates;

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
  client-ui = builds.clientUi;

  local-api-tests = crates.local-api.tests;
  installer-tests = crates.installer.tests;
  # Included from the day the crate landed, deliberately. An app nothing builds
  # is an app that is broken the next time anyone touches it, and this repo
  # already has one file sitting unverified because it was committed ahead of
  # its check.
  desktop-client-tests = crates.desktop-client.tests;

  clippy-local-api = crates.local-api.clippy;
  clippy-installer = crates.installer.clippy;
  clippy-desktop-client = crates.desktop-client.clippy;

  wg-register-tests =
    pkgs.runCommand "wg-register-tests"
    {
      nativeBuildInputs = [
        pkgs.busybox
        pkgs.jq
      ];
      src = ../apps/wg-register;
    }
    ''
      cp -r "$src" ./wg-register
      chmod -R +w ./wg-register
      busybox sh ./wg-register/setup_test.sh
      touch $out
    '';

  chart-checks =
    pkgs.runCommand "chart-checks"
    {
      nativeBuildInputs = [
        pkgs.kubernetes-helm
        (pkgs.python3.withPackages (ps: [ps.pyyaml]))
      ];
      src = ../apps/catalog;
    }
    ''
      cp -r "$src" ./catalog
      chmod -R +w ./catalog
      # helm needs a writable home, and the sandbox has none.
      export HOME=$PWD/home
      mkdir -p "$HOME"
      python3 ./catalog/check_charts.py
      touch $out
    '';

  coverage-local-api = crates.local-api.coverage;
  coverage-installer = crates.installer.coverage;

  nixos-create = toplevel "yolab-ci";
  nixos-join = toplevel "yolab-ci-join";
  nixos-wsl = toplevel "yolab-wsl";

  # The Nix<->Rust binary contract. Every `Command::new("x")` /
  # `Host::run_cmd("x", ...)` call site in local-api names an OS binary the
  # crate assumes is on PATH at runtime — and nothing else checks that: a
  # typo, or a shell-out added without adding it to any systemd unit's `path`,
  # compiles, passes clippy and cargo test, and only fails on a real node.
  #
  # `allBins` is the union of every yolab-owned systemd unit's own `path`
  # (each unit's PATH is that list PLUS the default `/run/current-system/sw`,
  # which is `environment.systemPackages` — see common.nix's yolab-local-api
  # unit, the one exception, for why that default cannot just be assumed
  # instead of read from real config) — built from the REAL Nix values every
  # unit already uses, not a hand-maintained parallel list, so it can't drift
  # from what a node actually gets.
  binary-contract = let
    allBins = pkgs.symlinkJoin {
      name = "yolab-all-unit-bins";
      paths =
        nixosSystems.yolab-ci.config.environment.systemPackages
        ++ pkgs.lib.concatMap (u: u.path or []) (
          builtins.attrValues nixosSystems.yolab-ci.config.systemd.services
        );
    };
  in
    pkgs.runCommand "binary-contract" {nativeBuildInputs = [pkgs.gnugrep];} ''
      grep -rhoE 'Command::new\("[^"]+"\)|\.run_cmd\(\s*"[^"]+"' \
        ${treeSrc}/homelab/local-api/src \
        | grep -oE '"[^"]+"' | tr -d '"' | sort -u > "$TMPDIR/needed.txt"

      missing=""
      while read -r bin; do
        [ -e "${allBins}/bin/$bin" ] || missing="$missing $bin"
      done < "$TMPDIR/needed.txt"

      if [ -n "$missing" ]; then
        echo "local-api shells out to these binaries, but no yolab-owned" >&2
        echo "systemd unit's path (or environment.systemPackages) provides" >&2
        echo "them:$missing" >&2
        exit 1
      fi
      touch $out
    '';

  disko-create = nixosSystems.yolab-ci.config.system.build.diskoScript;
  disko-join = nixosSystems.yolab-ci-join.config.system.build.diskoScript;
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
    grep -qx 'yolab-containerd-store.service' ${pkgs.writeText "k3s-after" (builtins.concatStringsSep "\n" nixosSystems.yolab-ci.config.systemd.services.k3s.after)} || {
      echo "k3s.service is no longer After= the store unit — re-read why this check exists" >&2
      exit 1
    }
    touch $out
  '';

  # A TIMER CANNOT RE-ARM WHILE THE UNIT IT TRIGGERS IS STILL ACTIVE.
  #
  # systemd moves a timer back to TIMER_WAITING — the only state from which it
  # computes a next elapse — when the unit it triggers becomes inactive or
  # failed, and at no other moment (timer.c, `timer_trigger_notify`). A
  # `RemainAfterExit = true` oneshot that succeeds stays "active (exited)" for
  # the rest of the boot, so a timer pointed at one fires exactly once and then
  # reports `NextElapseUSecMonotonic=infinity` forever.
  #
  # The timer base has nothing to do with it. It decides what the next elapse is
  # computed *from*; it does not decide whether the timer is re-armed at all.
  # This check used to say the same thing about OnUnitActiveSec/OnUnitInactiveSec
  # only, and told you to "use OnCalendar instead" — which three units then did,
  # and all three stayed just as dead. That is the 2026-09-06 outage: node1's NIC
  # flapped, the images RBD timed out, XFS shut the containerd data-root down,
  # and the "mounted but unreadable -> rebuild" recovery in
  # homelab/local-api/src/storage/containerd_store.rs never ran again, because
  # yolab-containerd-store.timer had not fired in two days. `systemctl
  # list-timers` on the live node: `NEXT: -` for all three OnCalendar timers
  # whose service was RemainAfterExit, next elapse scheduled for all the others.
  # The node went NotReady, its pods hung in Terminating, and every app in the UI
  # read "Starting up…" for 32 hours.
  #
  # So the invariant is about the service, not the timer base: if a timer exists
  # to retry or to re-check something, its service must be able to go inactive.
  #
  # `allowlist` is for units where "stop after the first success" is correct, not
  # a bug — see each one's own comment (yolab-ceph-bootstrap: a successful join
  # has nothing left to retry, and RemainAfterExit is load-bearing there because
  # ceph-mon requires it).
  self-healing-timers-can-re-arm = let
    allowlist = ["yolab-ceph-bootstrap"];
    services = nixosSystems.yolab-ci.config.systemd.services;
    timers = nixosSystems.yolab-ci.config.systemd.timers;
    remainsAfterExit = name: (services.${name}.serviceConfig.RemainAfterExit or false) == true;
    offenders = builtins.filter (
      name:
        services ? ${name}
        && remainsAfterExit name
        && !(builtins.elem name allowlist)
    ) (builtins.attrNames timers);
  in
    pkgs.runCommand "self-healing-timers-can-re-arm" {} ''
      offenders=${pkgs.writeText "offenders" (builtins.concatStringsSep "\n" offenders)}
      if [ -s "$offenders" ]; then
        echo "These units have a timer AND RemainAfterExit=true on the service." >&2
        echo "systemd re-arms a timer only when the unit it triggers goes" >&2
        echo "inactive or failed, so each of these fires once per boot and then" >&2
        echo "never again — whatever the timer base says. Drop RemainAfterExit," >&2
        echo "or add the unit to this check's allowlist with a comment saying why" >&2
        echo "running exactly once is actually correct there:" >&2
        cat "$offenders" >&2
        exit 1
      fi
      touch $out
    '';

  # A RETRY TIMER MUST MEASURE FROM WHEN THE RUN ENDED.
  #
  # The sibling check above is about whether a timer re-arms at all. This one is
  # about what happens once it does, and it exists because fixing the first bug
  # exposed the second within hours.
  #
  # OnCalendar and OnUnitActiveSec both compute the next elapse from a moment at
  # or before the run's START — the last trigger, and the last activation. So a
  # run that outlives its own interval finishes with the next elapse already in
  # the past, and systemd fires it again in the same second, forever. On
  # 2026-09-07 yolab-containerd-store (TimeoutStartSec 3600s, interval 5min) ran
  # 09:59:48 -> 10:17:41 moving 8.3G, and re-triggered at 10:17:41. Every run
  # stops k3s to do its work, so node2 never came back up.
  #
  # OnUnitInactiveSec measures from the moment the unit went inactive, which is
  # immune to that by construction and — since a failed unit also ends inactive —
  # already covers the failed-attempt case OnUnitActiveSec used to be paired in
  # for. For a timer that exists to retry or re-check something, it is simply the
  # right directive, and there is no interval arithmetic to get wrong.
  #
  # Scoped to `yolab-` units on purpose. Upstream nixpkgs ships genuine
  # wall-clock jobs (fstrim, logrotate, nix-gc) where OnCalendar is exactly
  # right and the work is bounded well under the period; this invariant is about
  # our own retry/reconcile timers, which are a different kind of thing.
  #
  # `allowlist` is for one of ours that genuinely wants a wall clock — a nightly
  # job that must run at a fixed hour rather than N after the last one. There are
  # none today. If you add one, make sure its service cannot outlive the gap.
  retry-timers-measure-from-run-end = let
    allowlist = [];
    timers = nixosSystems.yolab-ci.config.systemd.timers;
    fromStartBase = name: let
      tc = timers.${name}.timerConfig or {};
    in
      (tc ? OnCalendar) || (tc ? OnUnitActiveSec);
    offenders = builtins.filter (
      name:
        (builtins.match "yolab-.*" name != null)
        && fromStartBase name
        && !(builtins.elem name allowlist)
    ) (builtins.attrNames timers);
  in
    pkgs.runCommand "retry-timers-measure-from-run-end" {} ''
      offenders=${pkgs.writeText "offenders" (builtins.concatStringsSep "\n" offenders)}
      if [ -s "$offenders" ]; then
        echo "These timers use OnCalendar or OnUnitActiveSec. Both count from the" >&2
        echo "START of the run, so a run that outlives its interval finishes with" >&2
        echo "the next elapse already past and re-fires immediately — a hot loop," >&2
        echo "forever. Use OnUnitInactiveSec, which counts from when the run ended" >&2
        echo "and also covers failed attempts, or add the unit to this check's" >&2
        echo "allowlist if it truly needs a wall-clock schedule:" >&2
        cat "$offenders" >&2
        exit 1
      fi
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
    pkgs.runCommand "shellcheck"
    {
      nativeBuildInputs = [pkgs.shellcheck];
    }
    ''
      find ${treeSrc} -name '*.sh' -print0 | xargs -0 shellcheck -x
      touch $out
    '';

  # DL3018 wants every apk package pinned. These images track upstream Alpine
  # deliberately, and the wg-* tools must match the host kernel's WireGuard, so
  # pinning buys a stale userland rather than safety.
  hadolint =
    pkgs.runCommand "hadolint"
    {
      nativeBuildInputs = [pkgs.hadolint];
    }
    ''
      find ${treeSrc} -name 'Dockerfile' -print0 \
        | xargs -0 hadolint --ignore DL3018
      touch $out
    '';

  # statix is deliberately absent: its 39 findings are all "avoid repeated keys
  # in attribute sets", and flattening `boot.loader.grub.*` is not obviously an
  # improvement. `nix run nixpkgs#statix -- check` if you want it.
  deadnix =
    pkgs.runCommand "deadnix"
    {
      nativeBuildInputs = [pkgs.deadnix];
    }
    ''
      deadnix --fail ${treeSrc}
      touch $out
    '';
}
