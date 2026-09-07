# Two-node VM test: the topology most people actually run.
#
# nix/tests/boot.nix proved one machine boots. This proves two machines become a
# cluster, and — the part that matters — that the image store actually lands on
# Ceph on BOTH of them.
#
# WHY THIS EXISTS. On 2026-09-07 a reboot of a live two-node cluster left both
# machines unable to come back for over an hour, and the three bugs behind it had
# all been shipped by people reasoning carefully about code they never watched
# run:
#
#   1. yolab-containerd-store.timer never re-armed (RemainAfterExit), so the
#      "mounted but unreadable -> rebuild" recovery could not fire. 32h outage.
#   2. Fixing that with OnCalendar made it re-fire the instant each run ended,
#      because a calendar timer counts from the last TRIGGER; every run stops
#      k3s, so the node never came back.
#   3. The migration could not have finished regardless: the `cp` of the image
#      store was bounded by the generic 600s command timeout and needed ~1000s.
#
# Every one of those three is caught by the assertions below, and none of them
# needs fault injection to catch — they all show up in a plain, healthy boot of
# two machines. That is the uncomfortable part: this test would have caught all
# three on the day each was written, and the migration path it exercises had
# never once run to completion on any real machine.
#
# WIREGUARD IS STUBBED, DELIBERATELY. In production the cluster mesh is wg1,
# carrying each node's `sub_ipv6_private` to a peer via an external WireGuard
# server that does not exist in a VM. The mesh's *addressing* is what everything
# here depends on — Ceph binds mons to it, k3s advertises on it, local-api fans
# out over it — so the test puts the same addresses on the VMs' shared LAN and
# stubs the unit that would otherwise create them. What is under test is Ceph,
# k3s and the image store; the tunnel has its own coverage.
{
  pkgs,
  inputs,
  rust,
  disko,
}: let
  # Same shape as boot.nix's: the VM boots the harness's own root image, so the
  # install-time LVM layout is neutralised and GRUB pointed at the virtual disk.
  mkNode = {
    configPath,
    meshAddr,
  }: {lib, ...}: {
    # The same four the real `nixosSystem` passes as specialArgs (see flake.nix).
    # `localApiEnv` is easy to forget because nothing references it until a module
    # deep in common.nix builds a unit's ExecStart from it, and the resulting
    # "attribute 'localApiEnv' missing" surfaces from inside nixpkgs' module
    # system rather than from anything in this file.
    _module.args = {
      inherit inputs rust;
      yolabConfigPath = configPath;
      localApiEnv = rust.crates.local-api.package;
    };
    imports = [
      disko.nixosModules.disko
      ../../homelab/nixos/configuration.nix
      ../../homelab/nixos/disk-config.nix
    ];

    disko.devices = lib.mkForce {};
    boot.loader.grub.enable = lib.mkForce true;
    boot.loader.grub.device = lib.mkForce "/dev/vda";
    boot.loader.grub.efiSupport = lib.mkForce false;

    # Ceph mon + OSD + a k3s server + containerd is not a small machine, and an
    # OOM here looks like a mysterious hang rather than an obvious failure.
    virtualisation.memorySize = 4096;
    virtualisation.cores = 2;
    # The OSD's disk. Separate from the root image on purpose: an OSD on a
    # loopback file inside the root fs is a different code path from a real
    # block device, and the real one is what ships.
    virtualisation.emptyDiskImages = [8192];

    # ── The mesh, without WireGuard ──────────────────────────────────────────
    #
    # mkForce {} removes the wg1 *interface definition*, and with it the unit
    # that would try to reach a WireGuard server that is not in this test. The
    # address it would have configured is placed on the shared LAN instead, with
    # the same prefix length, so `fd00:cafe::/112` is on-link exactly as the
    # postSetup route in common.nix arranges it in production.
    networking.wireguard.interfaces = lib.mkForce {};
    networking.interfaces.eth1.ipv6.addresses = [
      {
        address = meshAddr;
        prefixLength = 112;
      }
    ];
    # Several units are ordered `after` this. Ordering against a unit that does
    # not exist is silently a no-op, which would let them race the address being
    # configured; a stub that genuinely completes once the mesh is up keeps the
    # ordering real.
    systemd.services.wireguard-wg1 = {
      description = "Stub for the WireGuard mesh (two-node VM test)";
      wantedBy = ["multi-user.target"];
      after = ["network-online.target"];
      wants = ["network-online.target"];
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
        TimeoutStartSec = "120s";
        ExecStart = pkgs.writeShellScript "await-mesh" ''
          for _ in $(seq 1 60); do
            ${pkgs.iproute2}/bin/ip -6 addr show dev eth1 \
              | ${pkgs.gnugrep}/bin/grep -q '${meshAddr}' && exit 0
            sleep 2
          done
          echo "mesh address ${meshAddr} never appeared on eth1" >&2
          exit 1
        '';
      };
    };

    # No route to the internet in the test VM, and Ceph/k3s must not wait on one.
    networking.useDHCP = lib.mkDefault false;

    # The test drives local-api's HTTP API to switch a disk on, the same way the
    # Storage page does. Nothing else in the image needs these.
    environment.systemPackages = [pkgs.curl pkgs.jq];

    # `yolabConfigPath` is what Nix EVALUATES the config from; this is where the
    # running system READS it at runtime, and they are not the same thing. Several
    # units resolve `${config.yolab.repoPath}/homelab/ignored/config.toml` (default
    # /etc/nixos) and parse it themselves — the Ceph join is one, and it needs
    # tunnel.account_token from there to authenticate to the seed node.
    #
    # Without this the join fails with "no tunnel.account_token in
    # /etc/nixos/homelab/ignored/config.toml — cannot authenticate", retries every
    # two minutes forever, and node2's mon never starts because it is `requiredBy`
    # the join. A real install has the repo checked out at that path, so this is
    # the harness standing in for it rather than a fixture inventing anything.
    environment.etc."nixos/homelab/ignored/config.toml".source = configPath;
  };
in
  pkgs.testers.nixosTest {
    name = "yolab-two-node";

    nodes.node1 = mkNode {
      configPath = ../../homelab/tests/boot-config.toml;
      meshAddr = "fd00:cafe::1";
    };
    nodes.node2 = mkNode {
      configPath = ../../homelab/tests/two-node-2.toml;
      meshAddr = "fd00:cafe::2";
    };

    testScript = ''
      import re

      start_all()

      # ── The mesh ──────────────────────────────────────────────────────────
      for m in (node1, node2):
          m.wait_for_unit("multi-user.target", timeout=900)
      node1.succeed("ping -6 -c3 fd00:cafe::2")
      node2.succeed("ping -6 -c3 fd00:cafe::1")

      # ── Ceph forms one cluster, not two ───────────────────────────────────
      #
      # Two nodes each bootstrapping their own cluster looks healthy on both
      # machines and is only discovered when the storage turns out to be split,
      # so assert the membership rather than the health.
      # wait_until_succeeds, NOT wait_for_unit. wait_for_unit aborts the moment it
      # sees a unit "inactive with no pending jobs" — which is precisely the state
      # of anything that comes up on a retry timer rather than in the boot
      # transaction. The joining node's mon is `requiredBy` a bootstrap that
      # legitimately fails until the seed is reachable, so it sits inactive
      # between attempts and wait_for_unit calls that a permanent failure.
      node1.wait_until_succeeds(
          "systemctl is-active ceph-mon-yolab-n1.service", timeout=600
      )
      node2.wait_until_succeeds(
          "systemctl is-active ceph-mon-yolab-n2.service", timeout=900
      )
      node1.wait_until_succeeds(
          "ceph -s --connect-timeout 10 | grep -E 'quorum .*yolab-n1.*yolab-n2'",
          timeout=600,
      )

      fsids = {m.succeed("ceph fsid --connect-timeout 10").strip() for m in (node1, node2)}
      assert len(fsids) == 1, f"nodes disagree about the cluster fsid: {fsids}"

      # ── k3s: both nodes join one control plane ────────────────────────────
      node1.wait_until_succeeds("k3s kubectl get --raw /readyz", timeout=900)
      node1.wait_until_succeeds(
          "k3s kubectl get nodes -o name | wc -l | grep -qx 2", timeout=900
      )
      node1.wait_until_succeeds(
          "k3s kubectl wait --for=condition=Ready node --all --timeout=60s", timeout=900
      )

      # ── Stand in for the rook operator ────────────────────────────────────
      #
      # The disk reconciler keeps its config and status ConfigMaps in `rook-ceph`
      # — a name left over from when Ceph ran under Rook, and still where rook's
      # CSI drivers live. On a real install that namespace is created by the
      # HelmChart in homelab/nixos/rook/operator.yaml (`createNamespace: true`),
      # which pulls the chart from https://charts.rook.io/release.
      #
      # A NixOS VM test has no internet, so that chart never installs and the
      # namespace never appears. Left alone, `auto_register_all_disks` fails with
      # "namespaces \"rook-ceph\" not found" every ~33s forever, no disk is ever
      # registered, and nothing downstream of an OSD can be reached. Creating it
      # here is the harness substituting for a component that is out of this
      # test's scope, not a fixture papering over a defect.
      node1.succeed(
          "k3s kubectl create namespace rook-ceph "
          "--dry-run=client -o yaml | k3s kubectl apply -f -"
      )

      # ── Switch a disk on, the way a person would ──────────────────────────
      #
      # Without this there is no OSD, so the images pool is never created, so the
      # RBD never exists and the whole point of the test is unreachable. In
      # production a human toggles the disk on the Storage page; nothing switches
      # one on by itself, deliberately — an OSD is a destructive, opt-in action.
      #
      # Driven through the real HTTP API rather than by writing the ConfigMap
      # directly, so the test exercises the path the UI actually takes. Auth is
      # the shared cluster token, the same header node-to-node calls use.
      TOKEN = "test-account-token"
      AUTH = f"-H 'x-yolab-cluster: {TOKEN}'"
      API = "http://[::1]:3001"

      node1.wait_for_open_port(3001, timeout=300)
      node1.wait_until_succeeds(f"curl -sf {AUTH} {API}/api/disks", timeout=300)

      # The inventory is keyed by node and published by each node's disk
      # reconciler into a ConfigMap, so it is empty until both have reported —
      # which is after the API is up, not at the same time. Wait for the data
      # rather than for the endpoint.
      #
      # Neither the node key nor the disk id is hardcoded. Both are internal
      # formats (the id has two shapes, and a writer changed once while its
      # reader did not — see split_record_key), so the test reads back whatever
      # the API actually offers.
      SPARE = (
          "select(.is_our_osd==false and .has_partitions==false and .mounted==false)"
      )
      node1.wait_until_succeeds(
          f"curl -sf {AUTH} {API}/api/disks | "
          f"jq -e '[to_entries[] | select((.value | map({SPARE}) | length) > 0)] "
          "| length == 2'",
          timeout=600,
      )
      offered = node1.succeed(
          f"curl -sf {AUTH} {API}/api/disks | "
          f"jq -r 'to_entries[] as $n | $n.value[] | {SPARE} | \"\\($n.key) \\(.id)\"'"
      ).split("\n")

      switched_on = set()
      for line in (l.strip() for l in offered if l.strip()):
          node_name, disk_id = line.split()
          if node_name in switched_on:
              continue
          switched_on.add(node_name)
          node1.succeed(
              f"curl -sf -X PUT {AUTH} -H 'content-type: application/json' "
              f"-d '{{\"desired\":\"ON\"}}' {API}/api/disks/{node_name}/{disk_id}"
          )
      assert len(switched_on) == 2, f"expected a disk on each node, got {switched_on}"

      # One OSD per machine, both up. Until this holds there is no pool capacity
      # and the images RBD cannot exist.
      node1.wait_until_succeeds(
          "ceph osd stat --connect-timeout 10 | grep -E '2 osds: 2 up'", timeout=900
      )

      # ── THE REGRESSION THIS FILE EXISTS FOR ───────────────────────────────
      #
      # containerd's data-root has to end up on the RBD, on BOTH nodes. This is
      # the path that had never once completed on a real machine: the first
      # attempt fails open (Ceph has no OSD yet at boot), and everything then
      # depends on the timer firing again.
      #
      # The swap itself is seconds — it mkfs-es and mounts, and deliberately does
      # NOT copy the old store (see storage/containerd_store.rs). The timeout is
      # generous only because it has to cover the timer's own retry interval.
      for m in (node1, node2):
          m.wait_until_succeeds(
              "findmnt -rno SOURCE --mountpoint /var/lib/rancher/k3s/agent/containerd "
              "| grep -q '^/dev/rbd'",
              timeout=900,
          )
          # Mounted is not the same as working — see the module header of
          # storage/containerd_store.rs for the 17-hour outage that distinction
          # was written in blood for.
          m.succeed("ls /var/lib/rancher/k3s/agent/containerd/ >/dev/null")

      # And k3s has to be left running afterwards. The migration stops it to do
      # its work and is responsible for putting it back; when it failed to, the
      # node stayed down while the unit looped.
      for m in (node1, node2):
          m.succeed("systemctl is-active k3s.service")

      # ── Every self-healing timer must actually be armed ───────────────────
      #
      # `NEXT: -` with a stale `LAST` is precisely what a RemainAfterExit oneshot
      # produces, and it is invisible unless something looks. Two separate
      # outages came from exactly this state going unnoticed for days.
      for m in (node1, node2):
          for unit in (
              "yolab-containerd-store",
              "yolab-ceph-mgr-key",
              "yolab-ceph-mds-key",
              "yolab-images-rbd",
          ):
              nxt = m.succeed(
                  f"systemctl show {unit}.timer -p NextElapseUSecRealtime "
                  "-p NextElapseUSecMonotonic --value"
              ).split()
              assert any(v not in ("", "infinity") for v in nxt), (
                  f"{unit}.timer on {m.name} will never fire again "
                  f"(next elapse: {nxt!r}) — a timer whose service cannot go "
                  "inactive stops re-arming, see nix/checks.nix"
              )

      # ── ...and must not be re-firing back to back ─────────────────────────
      #
      # The other half of the same bug: a timer counting from the run's START
      # re-triggers the instant a long run ends, and each containerd-store run
      # stops k3s. Two consecutive triggers separated by ~0s is the signature.
      def last_trigger(m, unit):
          out = m.succeed(f"systemctl show {unit}.timer -p LastTriggerUSecMonotonic --value")
          return int(out.strip() or 0)

      for m in (node1, node2):
          before = last_trigger(m, "yolab-containerd-store")
          m.sleep(90)
          after = last_trigger(m, "yolab-containerd-store")
          if after != before:
              gap_s = (after - before) / 1e6
              assert gap_s > 30, (
                  f"yolab-containerd-store.timer on {m.name} re-fired {gap_s:.1f}s "
                  "after the previous run — a timer that measures from the start of "
                  "the run hot-loops once a run outlives its interval"
              )

      # ── The store is on Ceph, so growing the pool grows image space ───────
      #
      # The whole point of the design: this is what is bought with the coupling
      # to Ceph, so assert it rather than assume it.
      for m in (node1, node2):
          src = m.succeed(
              "findmnt -rno SOURCE --mountpoint /var/lib/rancher/k3s/agent/containerd"
          ).strip()
          assert re.match(r"^/dev/rbd\d+$", src), f"{m.name}: image store on {src}"
      node1.succeed("rbd ls images | grep -qx yolab-n1")
      node1.succeed("rbd ls images | grep -qx yolab-n2")
    '';
  }
