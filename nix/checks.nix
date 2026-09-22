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
        "storage/controllers.rs" = 11;
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
