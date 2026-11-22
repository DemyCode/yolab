{
  pkgs,
  inputs,
  disko,
  yolabSpecialArgs,
}: let
  testLib = import ./lib.nix {inherit pkgs;};
  mkNode = {
    configPath,
    meshAddr,
  }: {lib, ...}: {
    _module.args = yolabSpecialArgs configPath // {inherit inputs;};
    imports = [
      disko.nixosModules.disko
      ../../homelab/nixos/configuration.nix
      ../../homelab/nixos/disk-config.nix
      (testLib.mesh meshAddr)
      (testLib.machine {inherit configPath;})
    ];

    disko.devices = lib.mkForce {};
    boot.loader.grub.enable = lib.mkForce true;
    boot.loader.grub.device = lib.mkForce "/dev/vda";
    boot.loader.grub.efiSupport = lib.mkForce false;

    virtualisation.memorySize = 4096;
    virtualisation.cores = 2;
    virtualisation.emptyDiskImages = [8192 8192];
    virtualisation.diskSize = 8192;

    networking.useDHCP = lib.mkDefault false;

    environment.systemPackages = [pkgs.curl pkgs.jq];

    yolab.machineDir = "/etc/yolab-machine";
    environment.etc."yolab-machine/config.toml".source = configPath;
  };
in
  testLib.withNetwork (pkgs.testers.nixosTest {
    name = "yolab-two-node";

    nodes.node1 = mkNode {
      configPath = ../../homelab/tests/boot-config.toml;
      meshAddr = "fd00:cafe::1";
    };
    nodes.node2 = mkNode {
      configPath = ../../homelab/tests/two-node-2.toml;
      meshAddr = "fd00:cafe::2";
    };

    testScript =
      testLib.preamble
      + ''
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
        TOKEN = CLUSTER_TOKEN
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

        # FOUR OSDs, not two. Each machine makes its own system LV an OSD at boot
        # (the system-osd resource), and the loop above switched on one spare disk
        # per machine on top of that. This assertion read "2 osds" for as long as
        # the test could not run at all — back when nothing created the system LV,
        # so there was no system OSD to count.
        node1.wait_until_succeeds(
            "ceph osd stat --connect-timeout 10 | grep -E '4 osds: 4 up'", timeout=900
        )

        # ── THE REGRESSION THIS FILE EXISTS FOR ───────────────────────────────
        #
        for m in (node1, node2):
            m.wait_until_succeeds(
                "findmnt -rno SOURCE --mountpoint /var/lib/rancher/k3s/agent/containerd "
                "| grep -q '^/dev/rbd'",
                timeout=900,
            )
            m.succeed("ls /var/lib/rancher/k3s/agent/containerd/ >/dev/null")
            m.succeed("systemctl is-active k3s.service")
            mounted_at = int(m.succeed(
                "systemctl show -P ExecMainExitTimestampMonotonic yolab-image-store.service"
            ).strip())
            k3s_started_at = int(m.succeed(
                "systemctl show -P InactiveExitTimestampMonotonic k3s.service"
            ).strip())
            assert 0 < mounted_at <= k3s_started_at, (
                f"{m.name}: k3s started at {k3s_started_at}, before the image store was "
                f"mounted at {mounted_at}"
            )

        # ── Self-healing must keep running, and must not hot-loop ────────────
        #
        # These two assertions used to be written against systemd .timer units.
        # Commit 998caf8 dropped the timers when the controller runtime landed
        # and the assertions stayed, naming units that no longer existed — see
        # tests-name-units-that-exist in nix/checks/source.nix.
        with step([node1, node2], "yolabd keeps supervising and does not hot-loop"):
            assert_yolabd_is_ticking(
                [node1, node2],
                ["images-rbd", "system-osd", "ceph-keys"],
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
  })
