{
  pkgs,
  lib,
  cfg,
  failOn,
  ...
}: let
  svcs = cfg.systemd.services;
  k3s = svcs.k3s;
  k3sWaitsFor = (k3s.after or []) ++ (k3s.wants or []) ++ (k3s.requires or []);
in {
  k3s-starts-once-on-its-image-store = let
    store = svcs.yolab-image-store or {};
    unit = "yolab-image-store.service";
    otherStorage =
      builtins.filter (
        u: lib.hasPrefix "yolab-" u && u != unit && !(lib.hasPrefix "yolab-reset-wipe" u)
      )
      k3sWaitsFor;
    storeWaitsFor = lib.flatten [(store.after or []) (store.requires or []) (store.wants or [])];
  in
    failOn "k3s-starts-once-on-its-image-store" {
      problems =
        lib.optional (store == {}) "yolab-image-store is not a unit"
        ++ lib.optional (!(builtins.elem unit (k3s.requires or []))) "k3s does not Require ${unit}, so it can start on the root disk"
        ++ lib.optional (!(builtins.elem unit (k3s.after or []))) "k3s is not After=${unit}, so it can start before the store is mounted"
        ++ map (u: "k3s is ordered behind ${u}; the image store is the only storage it waits for") otherStorage
        ++ lib.optional (builtins.elem "k3s.service" storeWaitsFor) "yolab-image-store waits for k3s, which waits for it"
        ++ lib.optional ((k3s.unitConfig.ConditionPathIsMountPoint or "") != "/var/lib/rancher/k3s/agent/containerd") "k3s has no ConditionPathIsMountPoint on containerd's data-root, so a start from anywhere can run it on the root disk"
        ++ lib.optional (builtins.elem "multi-user.target" (k3s.wantedBy or [])) "k3s is wanted by multi-user.target, so boot starts it before the image store is mounted"
        ++ lib.optional ((store.serviceConfig.Type or "") != "simple") "yolab-image-store is not Type=simple, so its wait holds the boot transaction"
        ++ lib.optional ((store.serviceConfig.RemainAfterExit or false) != true) "yolab-image-store does not RemainAfterExit, so k3s's Requires= stops k3s when it exits"
        ++ lib.optional ((store.restartIfChanged or true) != false) "yolab-image-store reruns on a switch"
        ++ lib.optional (builtins.elem "k3s.service" (svcs.yolab-local-api.wants or [])) "yolab-local-api Wants=k3s.service, which starts k3s before the image store is mounted"
        ++ lib.optional (builtins.elem "k3s.service" (svcs.yolab-local-api.after or [])) "yolab-local-api is After=k3s.service, so nothing can report why k3s is waiting"
        ++ lib.optional (builtins.elem unit (svcs.yolab-local-api.after or [])) "yolab-local-api is After=${unit}, but it creates the OSD and the RBD the store waits for";
      explain = ''
        k3s starts only once this machine's image store is mounted from Ceph,
        so every pod starts once, on the store it keeps using:'';
    };

  ceph-is-released-before-it-stops = let
    host = cfg.networking.hostName;
    release = svcs.yolab-ceph-release or {};
    unit = "yolab-ceph-release.service";
    mustFollow = [
      "wireguard-wg1.service"
      "ceph-mon-${host}.service"
      "ceph-mds-${host}.service"
    ];
  in
    failOn "ceph-is-released-before-it-stops" {
      problems =
        lib.optional (release == {}) "yolab-ceph-release is not a unit"
        ++ map (u: "yolab-ceph-release is not After=${u}, so ${u} can stop while pods still write through it") (
          builtins.filter (u: !(builtins.elem u (release.after or []))) mustFollow
        )
        ++ lib.optional (!(builtins.elem unit (svcs."yolab-ceph-osd@".before or [])))
        "yolab-ceph-osd@ is not Before=${unit}, so OSDs can stop while pods still write through them"
        ++ lib.optional (!(lib.hasInfix "storage release" (release.serviceConfig.ExecStop or "")))
        "yolab-ceph-release's ExecStop does not run `local-api storage release`"
        ++ lib.optional ((release.stopIfChanged or true) != false)
        "yolab-ceph-release does not set stopIfChanged = false, so a switch would kill every pod"
        ++ lib.optional (builtins.elem unit k3sWaitsFor)
        "k3s waits for yolab-ceph-release, which waits for the OSDs";
      explain = ''
        On 2026-10-20 node3 stopped its MDS, its OSD and k3s in the same
        second. Pods kept their CephFS mounts, their writes could not be
        flushed, and files came back 0 bytes.'';
    };

  ceph-units-share-one-env = let
    env = cfg.yolab.ceph.env;
    units = ["yolab-local-api" "yolab-ceph-bootstrap" "yolab-image-store"];
    missing = unit: builtins.filter (k: (svcs.${unit}.environment.${k} or null) != env.${k}) (builtins.attrNames env);
    stray = unit: builtins.filter (k: lib.hasPrefix "YOLAB_CEPH_" k && !(env ? ${k})) (builtins.attrNames svcs.${unit}.environment);
  in
    failOn "ceph-units-share-one-env" {
      problems =
        lib.optional (!(env ? YOLAB_CEPH_FSID)) "yolab.ceph.env has no YOLAB_CEPH_FSID"
        ++ lib.concatMap (u: map (k: "${u} does not get ${k} from yolab.ceph.env") (missing u)) units
        ++ lib.concatMap (u: map (k: "${u} sets ${k} outside yolab.ceph.env") (stray u)) units;
    };

  kernel-mounts-ceph-with-aes256k-keys = let
    kernel = cfg.boot.kernelPackages.kernel.version;
  in
    failOn "kernel-mounts-ceph-with-aes256k-keys" {
      problems =
        lib.optional (lib.versionOlder kernel "7.0") "kernel ${kernel} cannot authenticate to Ceph with aes256k keys; CephFS and the images RBD are kernel clients";
    };

  lvm-never-scans-an-rbd = let
    conf = cfg.environment.etc."lvm/lvm.conf".source;
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
}
