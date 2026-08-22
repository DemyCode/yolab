# containerd's image store, backed by Ceph RBD.
#
# This is the whole point of moving Ceph out of Kubernetes: with the daemons
# running as host units, a node can map an RBD and mount it as containerd's
# data-root *before* containerd starts. Adding a disk then grows the space
# available for container images, not just for PVC data.
#
# Verified on real hardware before this was written: RBD -> ext4/xfs ->
# overlayfs with an upperdir on it all work, which is what containerd's
# overlayfs snapshotter requires.
#
# TWO PROPERTIES HERE ARE LOAD-BEARING — do not "simplify" either one:
#
# 1. size=1 on the pool. Every node needs its own unpacked copy of every image
#    it runs, so pooling images across nodes saves nothing; at 3 nodes the
#    topology controller sets size=3, which would cost 9x the bytes for data
#    that is re-downloadable from a registry.
#
# 2. The RBD is sized to capacity that really exists, never oversubscribed.
#    Kubelet's image GC (imageGCHighThresholdPercent, see k3s/kubelet-image-gc.yaml)
#    works by statfs on the image filesystem. Hand it a thin 2TB image over a
#    500GB pool and it reports 5% full forever, never garbage collects, and the
#    pool silently reaches full-ratio — at which point Ceph blocks writes for
#    *every* app on *every* node, not just image pulls. Today's equivalent
#    failure is one node's root filling with ext4 ENOSPC and GC recovering. The
#    thin version is strictly worse, so the size tracks real capacity and grows
#    only as the pool grows.
{
  config,
  lib,
  pkgs,
  ...
}:
with lib; let
  cfg = config.yolab.ceph.imagesStore;
  cephCfg = config.yolab.ceph;
  host = config.networking.hostName;

  # k3s's embedded containerd keeps its image store here. Mounting over this
  # path is what moves images off the root disk.
  containerdRoot = "/var/lib/rancher/k3s/agent/containerd";

  # Every unit here runs before k3s, so it can only use host binaries — which is
  # exactly why Ceph had to leave Kubernetes in the first place.
  cephPath = with pkgs; [
    ceph
    ceph-client
    xfsprogs
    e2fsprogs
    util-linux
    coreutils
    gnugrep
    gawk
    jq
  ];
in {
  options.yolab.ceph.imagesStore = {
    enable = mkEnableOption "back containerd's image store with a Ceph RBD";

    poolName = mkOption {
      type = types.str;
      default = "images";
    };

    # What fraction of the pool's free space this node's image store may claim.
    # With N nodes sharing one pool you cannot promise each of them the whole
    # thing: 3 x "500G available" against 500G means all three believe they have
    # room, all three fill, and the pool hits full-ratio anyway. This is the one
    # place a cursor survives — but it is live-adjustable, unlike an LVM split
    # frozen at install time.
    shareOfPool = mkOption {
      type = types.float;
      default = 0.25;
      description = "Fraction of total pool capacity this node's image RBD may claim.";
    };

    minSizeGb = mkOption {
      type = types.int;
      default = 40;
      description = "Never size the image below this — it must always hold the base system's images.";
    };

    filesystem = mkOption {
      type = types.enum ["xfs" "ext4"];
      default = "xfs";
      description = "xfs matches what containerd expects and what most k8s distros use.";
    };
  };

  config = mkIf (cephCfg.enable && cfg.enable) {
    # ── LVM must never scan an RBD ───────────────────────────────────────────
    #
    # Every LVM command reads every block device it can see, looking for PV
    # labels — and that includes /dev/rbd0, this node's container image store.
    #
    # Ceph *blocks* I/O rather than failing it when it cannot serve a read, and
    # krbd retries indefinitely, so scanning a stalled RBD parks `lvs` in
    # uninterruptible sleep. A D-state process cannot be killed: SIGKILL is
    # ignored, systemd gives up, and the leftovers stay in the unit's cgroup.
    #
    # Observed live on node1: eight leaked `lvs` processes, "Processes still
    # around after final SIGKILL", yolab-local-api unstoppable, and the
    # nixos-rebuild that was trying to stop it wedged for 17 minutes.
    #
    # It is a circular dependency, not merely a hang. ceph-volume runs lvs to
    # create an OSD; that OSD is what would make the cluster able to serve I/O
    # again; and the cluster being unable to serve I/O is what stalls the RBD
    # that lvs is blocked reading. Nothing breaks that loop from inside.
    #
    # No OSD ever lives on an RBD — they are created on real disks — so LVM has
    # no reason to read one at all. global_filter rather than filter because
    # only the former applies to every command, including the udev-triggered
    # scans, which is where this bites.
    #
    # Written in lvm.conf's flat `section/key` form to match how the upstream
    # NixOS module contributes its own settings; a second `devices { }` block
    # would be a duplicate section in the same file.
    environment.etc."lvm/lvm.conf".text = lib.mkAfter ''
      devices/global_filter = [ "r|^/dev/rbd[0-9]+|", "a|.*|" ]
    '';

    # ── Provision the pool and this node's image ─────────────────────────────
    systemd.services.yolab-images-rbd = {
      description = "Ensure the Ceph images pool and this node's RBD image exist";
      wantedBy = ["multi-user.target"];
      # No dependency on any OSD unit: OSD instances are enabled dynamically by
      # local-api, so there is no single unit to order against. The script waits
      # for an OSD to actually report `up` instead, which is the real condition.
      after = ["ceph-mon-${host}.service" "ceph-mgr-${host}.service"];
      requires = ["ceph-mon-${host}.service"];
      # Deliberately NOT RemainAfterExit: on a fresh cluster this runs before any
      # OSD exists and exits cleanly with nothing to do. It has to be able to run
      # again once the user switches a disk on, so the timer below re-runs it.
      serviceConfig.Type = "oneshot";
      path = cephPath;
      script = ''
        set -uo pipefail
        for _ in $(seq 1 90); do ceph -s >/dev/null 2>&1 && break; sleep 1; done
        if ! ceph -s >/dev/null 2>&1; then
          echo "ceph not reachable — nothing to provision yet"
          exit 0
        fi

        # A pool cannot hold anything until an OSD is up. On a fresh cluster
        # there are none until the user switches a disk ON in the UI and the
        # reconciler creates one, so this is an ordinary state to be in — exit
        # cleanly and let a later run pick it up, rather than failing the boot.
        for _ in $(seq 1 90); do
          ceph osd stat 2>/dev/null | grep -qE '[1-9][0-9]* up' && break
          sleep 1
        done
        if ! ceph osd stat 2>/dev/null | grep -qE '[1-9][0-9]* up'; then
          echo "no OSD is up yet — the images pool will be created once a disk is switched on"
          exit 0
        fi

        set -e
        if ! ceph osd pool ls | grep -qx ${cfg.poolName}; then
          ceph osd pool create ${cfg.poolName} 32 32
          ceph osd pool set ${cfg.poolName} size 1 --yes-i-really-mean-it
          ceph osd pool application enable ${cfg.poolName} rbd
          rbd pool init ${cfg.poolName}
        fi

        # Size from capacity that actually exists (see header, point 2).
        TOTAL_MB=$(ceph df -f json | jq -r '.stats.total_bytes / 1048576 | floor')
        WANT_MB=$(awk -v t="$TOTAL_MB" -v s=${toString cfg.shareOfPool} 'BEGIN{printf "%d", t*s}')
        MIN_MB=$(( ${toString cfg.minSizeGb} * 1024 ))
        # Explicit `if`, not `[ ... ] && ...`: under `set -e` that idiom exits the
        # script with status 1 whenever the test is false and it happens to be the
        # last statement. It is not last today, which makes it a trap for the next
        # edit rather than a bug now.
        if [ "$WANT_MB" -lt "$MIN_MB" ]; then WANT_MB=$MIN_MB; fi

        if ! rbd ls ${cfg.poolName} | grep -qx ${host}; then
          # krbd cannot map object-map/fast-diff/deep-flatten, so create with
          # only the features the kernel client supports. Getting this wrong
          # produces a map failure that reads like a permissions error —
          # confirmed the hard way on real hardware.
          rbd create ${cfg.poolName}/${host} --size "$WANT_MB" \
            --image-feature layering,exclusive-lock
        fi
      '';
    };

    # ── Map + mount it, before containerd can start ──────────────────────────
    systemd.services.yolab-containerd-store = {
      description = "Map the images RBD and mount it as containerd's data-root";
      wantedBy = ["multi-user.target"];
      # `after` only, never `requires`, on both sides. This unit must not fail
      # when Ceph has no OSDs yet, and k3s must not fail when this one does —
      # otherwise a fresh cluster deadlocks: k3s waits on the RBD, the RBD waits
      # on an OSD, and OSDs are created by the reconciler from ConfigMaps that
      # only exist once k3s is up. Failing open costs one boot cycle before
      # images move off the root disk; failing closed costs the whole node, with
      # no UI left to diagnose it from.
      after = ["yolab-images-rbd.service"];
      before = ["k3s.service"];
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
        # Never let this unit's failure propagate into k3s.
        SuccessExitStatus = "0 1";
      };
      path = cephPath;
      script = ''
        set -uo pipefail

        # Bail out cleanly whenever Ceph is not ready yet. containerd then comes
        # up on the root disk, exactly as it did before this feature existed,
        # and the next boot picks up the RBD once an OSD exists.
        if ! ceph -s >/dev/null 2>&1; then
          echo "ceph not reachable — leaving containerd on the root disk for this boot"
          exit 0
        fi
        if ! rbd ls ${cfg.poolName} 2>/dev/null | grep -qx ${host}; then
          echo "no ${cfg.poolName}/${host} image yet — leaving containerd on the root disk for this boot"
          exit 0
        fi

        if mountpoint -q ${containerdRoot}; then
          echo "${containerdRoot} is already mounted"
          exit 0
        fi

        # Idempotent: rbd map on an already-mapped image just reports it.
        DEV=$(rbd map ${cfg.poolName}/${host} 2>/dev/null || rbd showmapped --format json \
          | jq -r '.[] | select(.pool=="${cfg.poolName}" and .name=="${host}") | .device')
        if [ -z "$DEV" ]; then
          echo "could not map ${cfg.poolName}/${host} — leaving containerd on the root disk" >&2
          exit 0
        fi
        echo "images RBD mapped at $DEV"

        # Format only when genuinely blank — a reboot must reuse the existing
        # image, not silently discard every pulled layer.
        if ! blkid "$DEV" >/dev/null 2>&1; then
          echo "no filesystem on $DEV, creating ${cfg.filesystem}"
          if ! ${
          if cfg.filesystem == "xfs"
          then "mkfs.xfs -f -m crc=1 \"$DEV\""
          else "mkfs.ext4 -q -m0 \"$DEV\""
        }; then
            echo "mkfs failed — leaving containerd on the root disk" >&2
            exit 0
          fi
        fi

        mkdir -p ${containerdRoot}

        # One-time migration off the root disk.
        #
        # On every boot after the first, k3s has already populated this directory
        # on root — the images pool does not exist until a disk is switched on,
        # which cannot happen before k3s runs. Mounting straight over it would
        # strand those layers: still consuming root, invisible, unreclaimable.
        # An earlier version refused to mount in that case, which meant the RBD
        # could never take over at all.
        #
        # Safe to do here because this unit runs Before=k3s, so containerd is not
        # running and nothing holds these files open.
        if [ -n "$(ls -A ${containerdRoot} 2>/dev/null)" ]; then
          echo "migrating the existing image store off the root disk"
          STAGE=$(mktemp -d)
          if ! mount "$DEV" "$STAGE"; then
            echo "could not mount $DEV for migration — staying on the root disk" >&2
            rmdir "$STAGE" 2>/dev/null || true
            exit 0
          fi
          # -a preserves hardlinks, xattrs and sparseness, all of which
          # containerd's content store relies on.
          if cp -a ${containerdRoot}/. "$STAGE"/; then
            umount "$STAGE"
            rmdir "$STAGE" 2>/dev/null || true
            # Remove and recreate rather than `rm -rf <dir>/*`: the glob misses
            # dotfiles, which would leave stale state behind for containerd to
            # trip over. (Do not write $\{dir:?} here — bash's guard syntax is
            # also Nix interpolation, and Nix wins, emitting a literal relative
            # path that silently deletes nothing.)
            rm -rf ${containerdRoot}
            mkdir -p ${containerdRoot}
            echo "migration complete, freed the copy on root"
          else
            # Most likely the image is smaller than the existing store. Roll
            # back rather than mount a half-populated store, which containerd
            # would read as a corrupt content store.
            echo "copy failed (is the RBD large enough?) — staying on the root disk" >&2
            umount "$STAGE" 2>/dev/null || true
            rmdir "$STAGE" 2>/dev/null || true
            exit 0
          fi
        fi

        if ! mount "$DEV" ${containerdRoot}; then
          echo "mount failed — leaving containerd on the root disk" >&2
          exit 0
        fi
        echo "containerd data-root now on $(findmnt -no SOURCE ${containerdRoot})"
      '';
    };

    # Ordered after the store unit so the mount lands before containerd opens
    # its data-root — but with `after` only, never `requires`. A hard dependency
    # here is what deadlocks a fresh cluster (see the note on that unit).
    systemd.services.k3s.after = ["yolab-containerd-store.service"];

    # ── Growth ───────────────────────────────────────────────────────────────
    # Without this the whole feature is inert: you would add a disk, the pool
    # would grow, and the image store would stay exactly the same size forever.
    systemd.services.yolab-images-rbd-grow = {
      description = "Grow the images RBD as the Ceph pool grows";
      # `requires`, not just `after`: the timer fires on a schedule and would
      # otherwise run during bootstrap, before the admin keyring exists, and
      # fail noisily with "unable to find a keyring" on a cluster that is
      # perfectly healthy — observed on the first live switch.
      after = ["yolab-containerd-store.service"];
      serviceConfig.Type = "oneshot";
      path = cephPath;
      script = ''
        set -uo pipefail
        # The timer fires on a schedule, so it can land during bootstrap before
        # the admin keyring exists, or on a boot where the store never mounted.
        # Both are normal states, not failures — observed live as a spurious
        # "unable to find a keyring" failure on a perfectly healthy cluster.
        if ! ceph -s >/dev/null 2>&1; then
          echo "ceph not reachable yet — nothing to grow"
          exit 0
        fi
        if ! mountpoint -q ${containerdRoot}; then
          echo "${containerdRoot} is not RBD-backed on this boot — nothing to grow"
          exit 0
        fi
        set -e
        CUR_MB=$(rbd info ${cfg.poolName}/${host} --format json | jq -r '.size / 1048576 | floor')
        TOTAL_MB=$(ceph df -f json | jq -r '.stats.total_bytes / 1048576 | floor')
        WANT_MB=$(awk -v t="$TOTAL_MB" -v s=${toString cfg.shareOfPool} 'BEGIN{printf "%d", t*s}')
        MIN_MB=$(( ${toString cfg.minSizeGb} * 1024 ))
        # Explicit `if`, not `[ ... ] && ...`: under `set -e` that idiom exits the
        # script with status 1 whenever the test is false and it happens to be the
        # last statement. It is not last today, which makes it a trap for the next
        # edit rather than a bug now.
        if [ "$WANT_MB" -lt "$MIN_MB" ]; then WANT_MB=$MIN_MB; fi

        # Only ever grow. Shrinking a mounted filesystem under a running
        # containerd would corrupt it, and a pool that shrank (a disk was
        # removed) is exactly when you least want to be truncating the image
        # store.
        if [ "$WANT_MB" -gt "$CUR_MB" ]; then
          echo "growing images RBD: ''${CUR_MB}MB -> ''${WANT_MB}MB"
          rbd resize ${cfg.poolName}/${host} --size "$WANT_MB"
          DEV=$(findmnt -no SOURCE ${containerdRoot})
          ${
          if cfg.filesystem == "xfs"
          then "xfs_growfs ${containerdRoot}"
          else "resize2fs \"$DEV\""
        }
        else
          echo "images RBD already at ''${CUR_MB}MB (target ''${WANT_MB}MB), nothing to do"
        fi
      '';
    };

    # Re-runs provisioning so the pool and image appear shortly after the first
    # disk is switched on, without needing a reboot to get there.
    systemd.timers.yolab-images-rbd = {
      wantedBy = ["timers.target"];
      timerConfig = {
        OnBootSec = "2min";
        OnUnitActiveSec = "5min";
      };
    };

    systemd.timers.yolab-images-rbd-grow = {
      wantedBy = ["timers.target"];
      timerConfig = {
        OnBootSec = "10min";
        OnUnitActiveSec = "1h";
      };
    };
  };
}
