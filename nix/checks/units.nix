{
  pkgs,
  lib,
  cfg,
  failOn,
  ...
}: let
  svcs = cfg.systemd.services;
  timers = cfg.systemd.timers;
  isOurs = name: lib.hasPrefix "yolab-" name;
in {
  self-healing-timers-can-re-arm = let
    allowlist = ["yolab-ceph-bootstrap"];
    remainsAfterExit = name: (svcs.${name}.serviceConfig.RemainAfterExit or false) == true;
  in
    failOn "self-healing-timers-can-re-arm" {
      problems = builtins.filter (
        name: svcs ? ${name} && remainsAfterExit name && !(builtins.elem name allowlist)
      ) (builtins.attrNames timers);
      explain = ''
        These units have a timer AND RemainAfterExit=true on the service.
        systemd re-arms a timer only when the unit it triggers goes
        inactive or failed, so each of these fires once per boot and then
        never again — whatever the timer base says. Drop RemainAfterExit,
        or add the unit to this check's allowlist with a comment saying why
        running exactly once is actually correct there:'';
    };

  retry-timers-measure-from-run-end = let
    allowlist = [];
    fromStartBase = name: let
      tc = timers.${name}.timerConfig or {};
    in
      (tc ? OnCalendar) || (tc ? OnUnitActiveSec);
  in
    failOn "retry-timers-measure-from-run-end" {
      problems = builtins.filter (
        name: isOurs name && fromStartBase name && !(builtins.elem name allowlist)
      ) (builtins.attrNames timers);
      explain = ''
        These timers use OnCalendar or OnUnitActiveSec. Both count from the
        START of the run, so a run that outlives its interval finishes with
        the next elapse already past and re-fires immediately — a hot loop,
        forever. Use OnUnitInactiveSec, which counts from when the run ended
        and also covers failed attempts, or add the unit to this check's
        allowlist if it truly needs a wall-clock schedule:'';
    };

  no-infinite-timeout-on-the-boot-line = let
    allowlist = [];
    directlyWanted = builtins.filter (
      n: builtins.elem "multi-user.target" (svcs.${n}.wantedBy or [])
    ) (builtins.attrNames svcs);
    deps = n:
      if svcs ? ${n}
      then map (lib.removeSuffix ".service") (svcs.${n}.after or [])
      else [];
    onBootLine = map (i: i.key) (builtins.genericClosure {
      startSet = map (n: {key = n;}) directlyWanted;
      operator = item: map (n: {key = n;}) (deps item.key);
    });
  in
    failOn "no-infinite-timeout-on-the-boot-line" {
      problems = builtins.filter (
        name:
          isOurs name
          && builtins.elem name onBootLine
          && (svcs.${name}.serviceConfig.TimeoutStartSec or null) == "infinity"
          && !(builtins.elem name allowlist)
      ) (builtins.attrNames svcs);
      explain = ''
        These units are on the boot line to multi-user.target (directly
        WantedBy it, or After= something that is) AND have
        TimeoutStartSec = "infinity". If the command they run can ever
        retry forever without giving up — and every yolab wait::until_ready
        loop can — the unit's start job never reaches a terminal state, and
        nothing ordered After= it can start either. This is exactly what
        held disk-loss-test's multi-user.target hostage on
        the storage resources in yolabd
        when a machine has no system LV: it is a real, tolerated state, not
        just a test artifact, and it used to mean the machine never finished
        booting. Bound the timeout — a unit that fails still fails loudly
        via 'systemctl --failed', it just also lets ordering resolve — or add
        the unit to this check's allowlist with a comment saying why nothing
        on the boot line can ever depend on it finishing:'';
    };

  ceph-survives-a-rebuild = let
    mustNotRestart = [
      "yolab-reset-wipe"
      "yolab-ceph-osd@"
      "yolab-ceph-bootstrap"
      "yolab-ceph-release"
    ];
    present = builtins.filter (n: svcs ? ${n}) mustNotRestart;
    isCephDaemon = unit: builtins.match "ceph-(mon|mgr|mds|osd)[-@].*" unit != null;
  in
    failOn "ceph-survives-a-rebuild" {
      problems =
        map (n: "${n} is on the must-not-restart list but is not a unit — rename or drop it") (
          lib.subtractLists present mustNotRestart
        )
        ++ map (n: "${n} does not set restartIfChanged = false") (
          builtins.filter (n: (svcs.${n}.restartIfChanged or true) != false) present
        )
        ++ builtins.concatMap (
          n: map (d: "${n} has Requires=${d}") (builtins.filter isCephDaemon (svcs.${n}.requires or []))
        ) (builtins.filter isOurs (builtins.attrNames svcs));
      explain = "A rebuild that restarts a Ceph daemon takes every OSD on the node with it:";
    };

  k3s-flags-exist-in-the-real-binary = let
    k3sPackage = cfg.services.k3s.package;
    flagNames = map (f: builtins.elemAt (lib.splitString "=" f) 0) cfg.services.k3s.extraFlags;
  in
    pkgs.runCommand "k3s-flags-exist-in-the-real-binary" {nativeBuildInputs = [pkgs.gnugrep];} ''
      ${k3sPackage}/bin/k3s server --help > help.txt 2>&1

      missing=""
      ${lib.concatMapStrings (flag: ''
          if ! grep -qE '(^|[[:space:]])${lib.escapeShellArg flag}([[:space:]]|,|$)' help.txt; then
            missing="$missing ${lib.escapeShellArg flag}"
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

  yolabd-migration-ratchet = let
    budget = [
      "yolab-banner"
      "yolab-caddy-credentials"
      "yolab-ceph-bootstrap"
      "yolab-ceph-noout"
      "yolab-ntfy-credentials"
      "yolab-reset-wipe"
    ];
    execOf = n: toString (svcs.${n}.serviceConfig.ExecStart or "");
    isOneshot = n: (svcs.${n}.serviceConfig.Type or "") == "oneshot";
    remaining = builtins.filter (
      n: isOurs n && isOneshot n && lib.hasInfix "local-api" (execOf n)
    ) (builtins.attrNames svcs);
    lines = names: pkgs.writeText "names" (lib.concatMapStrings (n: "${n}\n") (builtins.sort builtins.lessThan names));
  in
    pkgs.runCommand "yolabd-migration-ratchet" {nativeBuildInputs = [pkgs.diffutils];} ''
      if ! diff -u ${lines budget} ${lines remaining} > delta; then
        echo "The set of yolab systemd oneshots that shell out to local-api has" >&2
        echo "changed. This list is a ratchet: it may only shrink." >&2
        echo "" >&2
        echo "  '+' lines: a new oneshot doing convergent work in the boot" >&2
        echo "             transaction. A systemd job gets ONE attempt and is" >&2
        echo "             never retried, so anything that can legitimately be" >&2
        echo "             'not yet' belongs in yolabd's resource graph instead" >&2
        echo "             (homelab/local-api/src/runtime/resource.rs)." >&2
        echo "  '-' lines: you migrated one — delete the name from this list in" >&2
        echo "             nix/checks/units.nix in the same commit." >&2
        echo "" >&2
        cat delta >&2
        exit 1
      fi
      touch $out
    '';
}
