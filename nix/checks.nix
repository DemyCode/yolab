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
in let
  allChecks = {
    client-ui = builds.clientUi;
    client-ui-tests = builds.clientUiTests;

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
    # THE BOOT LINE TO K3S, pinned at the Nix level where no Rust test can see it:
    #
    #   yolab-ceph-system-osd → yolab-images-rbd → yolab-containerd-store → k3s
    #
    # Each arrow is After= AND Wants=: After alone orders a unit only if something
    # else starts it, Wants alone starts it without waiting. Losing either lets k3s
    # start with containerd's data-root on the root disk — the state the whole
    # store-moving machinery existed to undo, and removed so it can never be
    # entered. yolab-local-api must NOT be After=k3s: while k3s waits on storage,
    # local-api is how anyone sees why.
    containerd-store-after-order = let
      svcs = nixosSystems.yolab-ci.config.systemd.services;
      edges = [
        ["k3s" "yolab-containerd-store"]
        ["yolab-containerd-store" "yolab-images-rbd"]
        ["yolab-images-rbd" "yolab-ceph-system-osd"]
      ];
      missing = builtins.concatMap (e: let
        unit = builtins.elemAt e 0;
        dep = "${builtins.elemAt e 1}.service";
        s = svcs.${unit};
      in
        (pkgs.lib.optional (!(builtins.elem dep (s.after or []))) "${unit} is not After=${dep}")
        ++ (pkgs.lib.optional (!(builtins.elem dep (s.wants or []))) "${unit} does not Want=${dep}"))
      edges;
      problems =
        missing
        ++ pkgs.lib.optional (builtins.elem "k3s.service" (svcs.yolab-local-api.after or []))
        "yolab-local-api is After=k3s.service, so nothing can report why k3s is waiting";
    in
      pkgs.runCommand "containerd-store-after-order" {} ''
        ${pkgs.lib.concatMapStrings (p: "echo ${pkgs.lib.escapeShellArg p} >&2\n") problems}
        ${pkgs.lib.optionalString (problems != []) "exit 1"}
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

    # `ci-buckets-cover-every-check` used to live here: it diffed the hand-written
    # buckets in .github/workflows/push.yml against `checkNames`, because a check
    # missing from every bucket would silently stop running. Both halves are gone
    # — CI now runs `.#devour`, which builds whatever `checks` exposes, so there
    # is no list to drift and nothing left for that check to police.

    # THE API SURFACE IS WRITTEN DOWN TWICE, SO NEITHER COPY CAN DRIFT.
    #
    # `surface::ROUTE_TABLE` is the list every cross-cutting test walks — most
    # importantly `every_route_refuses_an_unauthenticated_stranger`, which is the
    # only thing standing between "someone added a route" and "someone added an
    # unauthenticated route". axum's `Router` cannot be enumerated at runtime, so
    # that table has to be written by hand, and a hand-written list of 69 things
    # is a list that silently falls behind.
    #
    # Both directions fail: a route in the router but not the table is a route no
    # sweep ever visits; a table entry naming no route is a test walking over
    # nothing and reporting success.
    route-table-is-complete =
      pkgs.runCommand "route-table-is-complete" {nativeBuildInputs = [pkgs.gnugrep pkgs.diffutils];}
      ''
        src=${treeSrc}/homelab/local-api/src

        # Every path build_router registers. Flattened first: eleven of these are
        # written across several lines, and the obvious grep — one that assumes
        # `.route("` is contiguous — silently finds 58 of the 69 and then reports
        # a clean diff against a table missing the same eleven.
        tr '\n' ' ' < "$src/router.rs" \
          | grep -oE '\.route\([[:space:]]*"[^"]+"' \
          | sed 's/^\.route([[:space:]]*"//; s/"$//' \
          | LC_ALL=C sort -u > registered

        # Every path the table claims.
        sed -n 's/^ *("\([^"]*\)", &\[.*$/\1/p' "$src/surface.rs" \
          | LC_ALL=C sort -u > tabled

        if ! diff -u tabled registered > delta; then
          echo "homelab/local-api/src/surface.rs ROUTE_TABLE no longer matches the" >&2
          echo "routes registered in homelab/local-api/src/router.rs." >&2
          echo "" >&2
          echo "  '+' lines: registered but NOT in the table — no test sweeps these," >&2
          echo "             including the one that checks they are behind auth." >&2
          echo "  '-' lines: in the table but registered nowhere — a test walking" >&2
          echo "             over a route that does not exist." >&2
          echo "" >&2
          cat delta >&2
          exit 1
        fi
        touch $out
      '';

    # THE SEAM RATCHET: direct machine access may only ever shrink.
    #
    # `host.rs` is the seam — every kubectl/ceph/systemctl/lsblk call is supposed
    # to go through the `Host` trait, because that is what lets a test substitute
    # `FakeHost` and drive the logic without a cluster. Code that names `RealHost`
    # or calls `crate::kubectl::` directly has stepped around it, and is
    # structurally untestable: there is no seam left to inject at.
    #
    # That is not hypothetical. `routers/restore.rs` is 1711 lines with 11 such
    # call sites, and on 2026-09-22 a restore finished pulling an app's data and
    # then never brought the app back up — the volume was restored, the Deployment
    # was never created, the record was marked "interrupted — scaled back up", and
    # `scaled_deployments` was empty so scaling back up did nothing. None of that
    # module's 20 tests could have caught it, because none of them can run a
    # restore at all. `heal/` is the counter-example: generic over `H: Host, N:
    # Network`, with both faked, and its failure paths are exercised.
    #
    # So this is a budget, per file, and the numbers may only go DOWN. Both
    # directions fail on purpose:
    #
    #   - over budget: the hole got deeper. Take a `host: &H` and call through the
    #     seam instead.
    #   - under budget: good — lower the number in the same commit, so the next
    #     person inherits the tighter bound rather than the slack.
    #
    # A file that reaches 0 comes off the list entirely; a file not on the list
    # may not have any.
    host-seam-ratchet = let
      # file -> how many direct `RealHost` / `crate::kubectl::` mentions it may
      # still have. Measured, not guessed. `host.rs` is the seam itself and
      # `kubectl.rs` defines the helpers, so neither is counted.
      budget = {
        "auth.rs" = 2;
        "boot/mod.rs" = 2;
        "charts.rs" = 5;
        "disks_reconciler.rs" = 2;
        "heal/credentials.rs" = 1;
        "heal/mod.rs" = 9;
        "mesh/mod.rs" = 5;
        "ops.rs" = 1;
        "routers/apps.rs" = 24;
        "routers/backup.rs" = 11;
        "routers/backup_common.rs" = 10;
        "routers/ceph.rs" = 2;
        "routers/disks.rs" = 4;
        "routers/restore.rs" = 11;
        "runtime/leader.rs" = 4;
        "storage/controllers.rs" = 11;
        "storage/mod.rs" = 2;
        "topology.rs" = 2;
      };
      # `attrNames` is already byte-sorted, which is what `LC_ALL=C sort` gives
      # the measured side. The two orderings have to agree or every line diffs.
      expected =
        pkgs.writeText "seam-budget"
        (pkgs.lib.concatStrings (
          map (n: "${n} ${toString budget.${n}}\n") (builtins.attrNames budget)
        ));
    in
      pkgs.runCommand "host-seam-ratchet" {nativeBuildInputs = [pkgs.gnugrep pkgs.diffutils];} ''
        # The subshell matters: `cd` lands in the read-only store path, so the
        # redirection below has to happen back in the build directory.
        (
          cd ${treeSrc}/homelab/local-api/src
          find . -name '*.rs' ! -path './host.rs' ! -path './kubectl.rs' -print0 \
            | xargs -0 grep -cE 'RealHost|crate::kubectl::' /dev/null
        ) \
          | grep -v ':0$' \
          | sed 's|^\./||; s|:| |' \
          | LC_ALL=C sort > actual

        if ! diff -u ${expected} actual > delta; then
          echo "Direct machine access moved. This list is a ratchet: it may only" >&2
          echo "shrink, and nix/checks.nix says why." >&2
          echo "" >&2
          echo "  '+' lines: more direct RealHost / crate::kubectl:: use than the" >&2
          echo "             budget allows, or a file that had none and now does." >&2
          echo "             Take a 'host: &H' and call through the seam instead," >&2
          echo "             the way homelab/local-api/src/heal/ does." >&2
          echo "  '-' lines: fewer than the budget — thank you. Lower the number" >&2
          echo "             in nix/checks.nix in this same commit." >&2
          echo "" >&2
          cat delta >&2
          exit 1
        fi
        touch $out
      '';

    # A NIXOS-REBUILD MUST NOT TAKE THE STORAGE DOWN WITH IT.
    #
    # Two separate mechanisms, one outage. On 2026-09-15 a rebuild on node1
    # changed yolab-ceph-bootstrap, systemd restarted ceph-mon because the mon
    # Requires it, and every OSD on the node stopped with the mon — `Requires`
    # propagates a STOP, which `restartIfChanged = false` on the OSD template
    # does nothing about, because the OSDs were not restarted, they were
    # dependency-stopped. Their restart then deadlocked on LVM (see
    # lvm-never-scans-an-rbd), and the node had no storage until someone
    # intervened.
    #
    # So both halves are asserted here:
    #
    #   1. Nothing of ours may `Requires=` a Ceph daemon. A daemon is a thing
    #      that comes and goes on its own; wanting one is fine, being stopped
    #      alongside one is not. The dependency in the other direction —
    #      `requiredBy` on a keyring unit, so the mon refuses to start without
    #      its key — is correct and is not what this catches.
    #
    #   2. The units that must not be cycled by a rebuild say so. Each one is on
    #      this list because restarting it mid-rebuild does real damage, not
    #      because restarting it is merely wasteful:
    #
    #        yolab-reset-wipe        erases this machine's cluster state
    #        yolab-ceph-osd@         cycles EVERY OSD on the node at once
    #        yolab-ceph-system-osd   re-runs OSD creation
    #        yolab-ceph-bootstrap    restarts the mon underneath the OSDs (1)
    #        yolab-images-rbd        unmaps the image store k3s is running from
    #        yolab-containerd-store  stops k3s to move several GB of images
    #
    # This replaces a comment in homelab/nixos/ceph/default.nix that explained
    # the whole thing and enforced none of it.
    ceph-survives-a-rebuild = let
      svcs = nixosSystems.yolab-ci.config.systemd.services;

      mustNotRestart = [
        "yolab-reset-wipe"
        "yolab-ceph-osd@"
        "yolab-ceph-system-osd"
        "yolab-ceph-bootstrap"
        "yolab-images-rbd"
        "yolab-containerd-store"
      ];
      missing = builtins.filter (n: !(svcs ? ${n})) mustNotRestart;
      restarted =
        builtins.filter
        (n: (svcs.${n}.restartIfChanged or true) != false)
        (builtins.filter (n: svcs ? ${n}) mustNotRestart);

      isCephDaemon = unit: builtins.match "ceph-(mon|mgr|mds|osd)[-@].*" unit != null;
      ours = builtins.filter (n: pkgs.lib.hasPrefix "yolab-" n) (builtins.attrNames svcs);
      hardDeps = builtins.concatMap (n:
        map (d: "${n} has Requires=${d}")
        (builtins.filter isCephDaemon (svcs.${n}.requires or [])))
      ours;

      problems =
        (map (n: "${n} is on the must-not-restart list but is not a unit — rename or drop it") missing)
        ++ (map (n: "${n} does not set restartIfChanged = false") restarted)
        ++ hardDeps;
    in
      pkgs.runCommand "ceph-survives-a-rebuild" {} ''
        ${pkgs.lib.concatMapStrings (p: "echo ${pkgs.lib.escapeShellArg p} >&2\n") problems}
        ${pkgs.lib.optionalString (problems != []) ''
          echo "" >&2
          echo "See the note on this check in nix/checks.nix: a rebuild that" >&2
          echo "restarts a Ceph daemon takes every OSD on the node with it." >&2
          exit 1
        ''}
        touch $out
      '';

    # LVM MUST NOT SCAN AN RBD — BY ANY OF ITS NAMES.
    #
    # ceph-volume runs `lvs` to create an OSD; `lvs` scanning /dev/rbd0 blocks in
    # io_getevents because the RBD cannot be served while the cluster has no OSD;
    # the OSD that would fix that is the one waiting on `lvs`. Observed on node1:
    # eight leaked `lvs` processes, yolab-local-api unstoppable, and a
    # nixos-rebuild wedged for 17 minutes trying to stop it.
    #
    # The first fix rejected `^/dev/rbd` only, and LVM went on scanning the same
    # device through /dev/block/253:0, which the trailing `a|.*|` happily
    # accepted — same deadlock, on 2026-09-15, via a name nobody had thought of.
    # THE LESSON IS THE ALIASES, so that is what this asserts: every directory
    # the kernel and udev publish a block device under must be rejected, and the
    # accept-everything rule must come last. A real disk still arrives by its
    # kernel name (/dev/sda, /dev/nvme0n1, /dev/dm-0) and is still accepted; an
    # RBD has no name left.
    #
    # No OSD ever lives on an RBD, so nothing legitimate is lost.
    lvm-never-scans-an-rbd = let
      conf = nixosSystems.yolab-ci.config.environment.etc."lvm/lvm.conf".source;
    in
      pkgs.runCommand "lvm-never-scans-an-rbd" {nativeBuildInputs = [pkgs.gnugrep];} ''
        filter=$(grep -E '^[[:space:]]*devices/global_filter' ${conf} | tail -1)
        if [ -z "$filter" ]; then
          echo "lvm.conf sets no devices/global_filter at all, so LVM scans" >&2
          echo "every block device on the node — including the image RBD." >&2
          exit 1
        fi
        echo "global_filter: $filter"

        missing=""
        for alias in '/dev/rbd' '/dev/block/' '/dev/disk/'; do
          case "$filter" in
            *"r|^$alias"*) ;;
            *) missing="$missing $alias" ;;
          esac
        done
        if [ -n "$missing" ]; then
          echo "devices/global_filter does not reject these names:$missing" >&2
          echo "" >&2
          echo "An RBD is reachable under every one of them. Rejecting only some" >&2
          echo "is the 2026-09-15 deadlock exactly: /dev/rbd0 was rejected and" >&2
          echo "/dev/block/253:0 was scanned anyway." >&2
          exit 1
        fi

        # `filter` rather than `global_filter` does not cover udev-triggered
        # scans, which is where this actually bites.
        case "$filter" in
          *'"a|.*|"'*) ;;
          *)
            echo "global_filter never accepts anything, so LVM would ignore the" >&2
            echo "real disks too. It needs a trailing a|.*| after the rejects." >&2
            exit 1
            ;;
        esac
        touch $out
      '';

    # EVERY APP IN THE CATALOG IS A WORD MOST PEOPLE HAVE NEVER SEEN.
    #
    # Vikunja, Karakeep, Miniflux, Navidrome. A grid of 74 of those is not a
    # shop, it is a wall — the one line in client-ui/src/catalog/meta.ts
    # comparing each to something the reader already pays for is what turns it
    # into somewhere you can find what you came for. That file is the
    # highest-leverage copy in the product and nothing connects it to the charts
    # it describes, so a chart added on one side is simply absent on the other.
    #
    # `taglineFor` falls back to the chart's own description, so a missing entry
    # is never blank — it is worse than that: it is a sentence written for
    # someone who already knows what the app is, sitting in the one place that
    # exists for someone who does not.
    #
    # Two directions, and they are not symmetric:
    #
    #   - A tagline naming no chart is dead copy. Zero today, and it stays zero:
    #     that is a hard failure, because the only way to get one is a rename or
    #     a delete that half-landed.
    #
    #   - A chart with no tagline is a gap, and there are 13. Listing them is
    #     what makes them finite: the list may only SHRINK, so the storefront
    #     cannot quietly get less curated as the catalog grows.
    catalog-apps-have-a-tagline = let
      # Charts still waiting for a line of their own. Delete a name when you
      # write one; never add one.
      uncurated = [
        "babybuddy"
        "emulatorjs"
        "healthchecks"
        "kimai"
        "mastodon"
        "onlyoffice"
        "pairdrop"
        "prowlarr"
        "radarr"
        "sonarr"
        "speedtest-tracker"
        "unifi"
        "your-spotify"
      ];
      expected = pkgs.writeText "uncurated" (
        pkgs.lib.concatStrings (map (n: "${n}\n") (builtins.sort builtins.lessThan uncurated))
      );
    in
      pkgs.runCommand "catalog-apps-have-a-tagline" {
        nativeBuildInputs = [pkgs.gnugrep pkgs.diffutils];
      } ''
        meta=${treeSrc}/homelab/client-ui/src/catalog/meta.ts

        # Every chart in the official catalog, library charts excluded.
        for chart in ${treeSrc}/apps/catalog/*/; do
          [ -f "$chart/Chart.yaml" ] || continue
          grep -q '^type: library' "$chart/Chart.yaml" && continue
          grep '^name:' "$chart/Chart.yaml" | head -1 | awk '{print $2}'
        done | LC_ALL=C sort -u > charts

        # Every key of APP_META. TWO SHAPES, and only matching one of them is
        # how this check was first written: 25 of the 61 entries are a single
        # line (`foo: { tagline: "...", group: "x" },`) and 36 span three, so an
        # extraction anchored to `: {$` finds 36 and reports 38 healthy charts as
        # missing copy. No `$` anchor here, deliberately.
        awk '/^export const APP_META/{f=1} f{print} /^};$/{if(f) exit}' "$meta" \
          | sed -n 's/^  "\?\([A-Za-z0-9][A-Za-z0-9._-]*\)"\?: {.*$/\1/p' \
          | LC_ALL=C sort -u > tagged

        dead=$(comm -13 charts tagged)
        if [ -n "$dead" ]; then
          echo "These taglines name a chart that does not exist — a rename or a" >&2
          echo "delete that only half landed:" >&2
          echo "$dead" >&2
          exit 1
        fi

        comm -23 charts tagged > missing
        if ! diff -u ${expected} missing > delta; then
          echo "The set of catalog apps with no tagline has changed." >&2
          echo "" >&2
          echo "  '+' lines: a new app with no storefront copy. Write one line in" >&2
          echo "             homelab/client-ui/src/catalog/meta.ts saying what it" >&2
          echo "             does and what it is like, or add it to this check's" >&2
          echo "             list if it genuinely has to ship uncurated." >&2
          echo "  '-' lines: you wrote one — delete the name from the list in" >&2
          echo "             nix/checks.nix in the same commit." >&2
          echo "" >&2
          cat delta >&2
          exit 1
        fi
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
  };
in
  allChecks
