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
    client-ui-lint = builds.clientUiLint;

    local-api-tests = crates.local-api.tests;
    installer-tests = crates.installer.tests;
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

    nixos-create = toplevel "yolab-ci";
    nixos-join = toplevel "yolab-ci-join";

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

    k3s-does-not-wait-for-storage = let
      svcs = nixosSystems.yolab-ci.config.systemd.services;
      k3s = svcs.k3s;
      ordering = (k3s.after or []) ++ (k3s.wants or []) ++ (k3s.requires or []);
      storageUnits =
        builtins.filter (
          u: pkgs.lib.hasPrefix "yolab-" u && !(pkgs.lib.hasPrefix "yolab-reset-wipe" u)
        )
        ordering;
      problems =
        (map (u: "k3s is ordered behind ${u}, so storage can hold the control plane down") storageUnits)
        ++ pkgs.lib.optional (builtins.elem "k3s.service" (svcs.yolab-local-api.after or []))
        "yolab-local-api is After=k3s.service, so nothing can report why k3s is waiting";
    in
      pkgs.runCommand "k3s-does-not-wait-for-storage" {} ''
        ${pkgs.lib.concatMapStrings (p: "echo ${pkgs.lib.escapeShellArg p} >&2\n") problems}
        ${pkgs.lib.optionalString (problems != []) ''
          echo "" >&2
          echo "k3s must boot whether or not Ceph is healthy. Storage convergence" >&2
          echo "belongs in yolabd's resource graph, which retries; a systemd job" >&2
          echo "gets one attempt and then holds everything ordered behind it." >&2
          exit 1
        ''}
        touch $out
      '';
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

    no-infinite-timeout-on-the-boot-line = let
      allowlist = [];
      svcs = nixosSystems.yolab-ci.config.systemd.services;
      stripService = name: pkgs.lib.removeSuffix ".service" name;

      # A unit is on the boot line if multi-user.target directly wants it, or
      # if a unit already on the boot line is After= it — a hang in an
      # ancestor's dependency blocks the ancestor's own start job exactly the
      # same way, transitively, all the way down.
      directlyWanted = builtins.filter (
        n: builtins.elem "multi-user.target" (svcs.${n}.wantedBy or [])
      ) (builtins.attrNames svcs);
      deps = n:
        if svcs ? ${n}
        then map stripService (svcs.${n}.after or [])
        else [];
      closureItems = builtins.genericClosure {
        startSet = map (n: {key = n;}) directlyWanted;
        operator = item: map (n: {key = n;}) (deps item.key);
      };
      onBootLine = map (i: i.key) closureItems;

      offenders = builtins.filter (
        name:
          (builtins.match "yolab-.*" name != null)
          && builtins.elem name onBootLine
          && (svcs.${name}.serviceConfig.TimeoutStartSec or null) == "infinity"
          && !(builtins.elem name allowlist)
      ) (builtins.attrNames svcs);
    in
      pkgs.runCommand "no-infinite-timeout-on-the-boot-line" {} ''
        offenders=${pkgs.writeText "offenders" (builtins.concatStringsSep "\n" offenders)}
        if [ -s "$offenders" ]; then
          echo "These units are on the boot line to multi-user.target (directly" >&2
          echo "WantedBy it, or After= something that is) AND have" >&2
          echo "TimeoutStartSec = \"infinity\". If the command they run can ever" >&2
          echo "retry forever without giving up — and every yolab wait::until_ready" >&2
          echo "loop can — the unit's start job never reaches a terminal state, and" >&2
          echo "nothing ordered After= it can start either. This is exactly what" >&2
          echo "held disk-loss-test's multi-user.target hostage on" >&2
          echo "the storage resources in yolabd" >&2
          echo "when a machine has no system LV: it is a real, tolerated state, not" >&2
          echo "just a test artifact, and it used to mean the machine never finished" >&2
          echo "booting. Bound the timeout — a unit that fails still fails loudly" >&2
          echo "via 'systemctl --failed', it just also lets ordering resolve — or add" >&2
          echo "the unit to this check's allowlist with a comment saying why nothing" >&2
          echo "on the boot line can ever depend on it finishing:" >&2
          cat "$offenders" >&2
          exit 1
        fi
        touch $out
      '';

    # POSIX for using the bash its own shebang asks for. shellcheck reads the
    shellcheck =
      pkgs.runCommand "shellcheck"
      {
        nativeBuildInputs = [pkgs.shellcheck];
      }
      ''
        find ${treeSrc} -name '*.sh' -print0 | xargs -0 shellcheck -x
        touch $out
      '';

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

    host-seam-ratchet = let
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

        "storage/mod.rs" = 2;
        "topology.rs" = 2;
      };
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

    ceph-survives-a-rebuild = let
      svcs = nixosSystems.yolab-ci.config.systemd.services;

      mustNotRestart = [
        "yolab-reset-wipe"
        "yolab-ceph-osd@"
        "yolab-ceph-bootstrap"
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

    k3s-flags-exist-in-the-real-binary = let
      k3sPackage = nixosSystems.yolab-ci.config.services.k3s.package;
      extraFlags = nixosSystems.yolab-ci.config.services.k3s.extraFlags;
      # "--foo=bar" -> "--foo"; a flag with no "=" (a bare boolean switch) is
      # unaffected.
      flagNames = map (f: builtins.elemAt (pkgs.lib.splitString "=" f) 0) extraFlags;
    in
      pkgs.runCommand "k3s-flags-exist-in-the-real-binary" {nativeBuildInputs = [pkgs.gnugrep];} ''
        # k3s server --help needs no cluster and no network: it parses its own
        # flag definitions and exits. This is the check that would have caught
        # the bad kubelet flag that took down an embedded control plane — a
        # flag that "should" work is only checked against the binary here, not
        # against anyone's memory of the CLI.
        ${k3sPackage}/bin/k3s server --help > help.txt 2>&1

        missing=""
        ${pkgs.lib.concatMapStrings (flag: ''
            if ! grep -qE '(^|[[:space:]])${pkgs.lib.escapeShellArg flag}([[:space:]]|,|$)' help.txt; then
              missing="$missing ${pkgs.lib.escapeShellArg flag}"
            fi
          '')
          flagNames}

        if [ -n "$missing" ]; then
          echo "These flags are in homelab/nixos/common.nix's services.k3s.extraFlags" >&2
          echo "but do not appear in '$(basename ${k3sPackage})/bin/k3s server --help':" >&2
          echo "$missing" >&2
          echo "" >&2
          echo "A flag k3s does not recognize is silently fatal — it refuses to" >&2
          echo "start rather than warning and continuing, taking the whole" >&2
          echo "embedded control plane down with it. Check the real binary," >&2
          echo "not what the flag used to be called." >&2
          exit 1
        fi
        touch $out
      '';

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

    catalog-apps-have-a-tagline = let
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
    vm-tests-give-swap-room = pkgs.runCommand "vm-tests-give-swap-room" {nativeBuildInputs = [pkgs.gnugrep];} ''
      problems=""
      for f in ${treeSrc}/nix/tests/*.nix; do
        grep -q 'boot.loader.grub.enable = lib.mkForce true' "$f" || continue

        size=$(grep -oE 'virtualisation\.diskSize = [0-9]+' "$f" | grep -oE '[0-9]+' | head -1)
        if [ -z "$size" ]; then
          problems="$problems\n$(basename "$f"): boots via grub but sets no virtualisation.diskSize"
        elif [ "$size" -lt 4096 ]; then
          problems="$problems\n$(basename "$f"): virtualisation.diskSize=$size is below the 4096 floor"
        fi
      done
      if [ -n "$problems" ]; then
        echo "The VM's root disk defaults to 'auto'-sized to the system" >&2
        echo "closure with zero slack (qemu-vm.nix's additionalSpace =" >&2
        echo "\"0M\", hardcoded, not exposed as an option). services.swapspace" >&2
        echo "(homelab/nixos/common.nix) creates its swapfiles at" >&2
        echo "/var/lib/swapspace, on that same root disk — with no slack" >&2
        echo "there, it can never allocate any swap, so real memory" >&2
        echo "pressure goes straight to the OOM killer instead of being" >&2
        echo "absorbed. This is exactly what OOM-killed coredns mid-test" >&2
        echo "once VM tests got real internet access and their k3s addons" >&2
        echo "started actually pulling and running real images (2026-09-22)." >&2
        echo "Set virtualisation.diskSize to at least 4096 (MiB) on any" >&2
        echo "grub-booted test node:" >&2
        printf "%b\n" "$problems" >&2
        exit 1
      fi
      touch $out
    '';

    yolabd-migration-ratchet = let
      budget = [
        "yolab-banner"
        "yolab-caddy-credentials"
        "yolab-ceph-bootstrap"
        "yolab-ceph-noout"
        "yolab-ntfy-credentials"
        "yolab-reset-wipe"
      ];
      svcs = nixosSystems.yolab-ci.config.systemd.services;
      execOf = n: toString (svcs.${n}.serviceConfig.ExecStart or "");
      isOneshot = n: (svcs.${n}.serviceConfig.Type or "") == "oneshot";
      remaining = builtins.filter (
        n:
          pkgs.lib.hasPrefix "yolab-" n
          && isOneshot n
          && pkgs.lib.hasInfix "local-api" (execOf n)
      ) (builtins.attrNames svcs);
      expected = pkgs.writeText "expected" (
        pkgs.lib.concatStrings (map (n: "${n}\n") (builtins.sort builtins.lessThan budget))
      );
      actual = pkgs.writeText "actual" (
        pkgs.lib.concatStrings (map (n: "${n}\n") (builtins.sort builtins.lessThan remaining))
      );
    in
      pkgs.runCommand "yolabd-migration-ratchet" {nativeBuildInputs = [pkgs.diffutils];} ''
        if ! diff -u ${expected} ${actual} > delta; then
          echo "The set of yolab systemd oneshots that shell out to local-api has" >&2
          echo "changed. This list is a ratchet: it may only shrink." >&2
          echo "" >&2
          echo "  '+' lines: a new oneshot doing convergent work in the boot" >&2
          echo "             transaction. A systemd job gets ONE attempt and is" >&2
          echo "             never retried, so anything that can legitimately be" >&2
          echo "             'not yet' belongs in yolabd's resource graph instead" >&2
          echo "             (homelab/local-api/src/runtime/resource.rs)." >&2
          echo "  '-' lines: you migrated one — delete the name from this list in" >&2
          echo "             nix/checks.nix in the same commit." >&2
          echo "" >&2
          cat delta >&2
          exit 1
        fi
        touch $out
      '';

    tests-name-units-that-exist = let
      svcs = builtins.attrNames nixosSystems.yolab-ci.config.systemd.services;
      timers = builtins.attrNames nixosSystems.yolab-ci.config.systemd.timers;
      known = pkgs.writeText "known-units" (
        pkgs.lib.concatStrings (
          (map (n: "${n}.service\n") svcs) ++ (map (n: "${n}.timer\n") timers)
        )
      );
    in
      pkgs.runCommand "tests-name-units-that-exist" {
        nativeBuildInputs = [pkgs.gnugrep pkgs.coreutils];
      } ''
        LC_ALL=C sort -u ${known} > known

        grep -rhoE '[A-Za-z0-9@_.-]+\.(service|timer)' \
          ${treeSrc}/nix/tests ${treeSrc}/nix/checks.nix \
          | grep -E '^yolab-' \
          | LC_ALL=C sort -u > named

        if ! comm -23 named known > ghosts; then
          echo "could not compare unit names" >&2
          exit 1
        fi

        if [ -s ghosts ]; then
          echo "These unit names appear in nix/tests or nix/checks.nix but no" >&2
          echo "such unit exists in the evaluated NixOS config:" >&2
          echo "" >&2
          sed 's/^/  /' ghosts >&2
          echo "" >&2
          echo "A VM test that waits on, or asserts about, a unit that was" >&2
          echo "renamed or deleted does not fail loudly — systemctl answers for" >&2
          echo "a unit that does not exist, so the assertion quietly stops" >&2
          echo "meaning anything. Every timer assertion in two-node.nix and" >&2
          echo "reboot.nix outlived the timers themselves by nine days this way." >&2
          exit 1
        fi
        touch $out
      '';

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
