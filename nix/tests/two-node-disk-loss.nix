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
      (testLib.machine {inherit configPath;})
    ];

    disko.devices = lib.mkForce {};
    boot.loader.grub.enable = lib.mkForce true;
    boot.loader.grub.device = lib.mkForce "/dev/vda";
    boot.loader.grub.efiSupport = lib.mkForce false;

    virtualisation.memorySize = 4096;
    virtualisation.cores = 2;
    virtualisation.emptyDiskImages = [8192 8192];

    networking.wireguard.interfaces = lib.mkForce {};
    networking.interfaces.eth1.ipv6.addresses = [
      {
        address = meshAddr;
        prefixLength = 112;
      }
    ];
    systemd.services.wireguard-wg1 = {
      description = "Stub for the WireGuard mesh (two-node disk-loss VM test)";
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

    networking.useDHCP = lib.mkDefault false;
    environment.systemPackages = [pkgs.curl pkgs.jq];
  };
in
  pkgs.testers.nixosTest {
    name = "yolab-two-node-disk-loss";

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
        TOKEN = "test-account-token"
        AUTH = f"-H 'x-yolab-cluster: {TOKEN}'"
        API = "http://[::1]:3001"
        K = "k3s kubectl"

        def jq_ok(node, cmd, expr, timeout):
            node.wait_until_succeeds(f"{cmd} | jq -e '{expr}' >/dev/null", timeout=timeout)

        start_all()

        with step([node1, node2], "the mesh and cluster come up, both nodes join"):
            for m in (node1, node2):
                m.wait_for_unit("multi-user.target", timeout=900)
            node1.wait_until_succeeds(
                "systemctl is-active ceph-mon-yolab-n1.service", timeout=600
            )
            node2.wait_until_succeeds(
                "systemctl is-active ceph-mon-yolab-n2.service", timeout=900
            )
            node1.wait_until_succeeds(
                "ceph -s --connect-timeout 10 | grep -E 'quorum .*yolab-n1.*yolab-n2|quorum .*yolab-n2.*yolab-n1'",
                timeout=600,
            )
            node1.wait_until_succeeds(f"{K} get --raw /readyz", timeout=900)
            node1.succeed(
                f"{K} create namespace rook-ceph --dry-run=client -o yaml | {K} apply -f -"
            )

        # ── Switch on one spare disk per node: 4 OSDs, spread across both
        #    machines — the shape a real 2-node install actually has ────────
        SPARE = "select(.is_our_osd==false and .has_partitions==false and .mounted==false)"
        with step(node1, "a spare disk on each node is switched on"):
            node1.wait_for_open_port(3001, timeout=300)
            jq_ok(node1, f"curl -sf {AUTH} {API}/api/disks", f"[.[][] | {SPARE}] | length == 2", 600)
            offered = node1.succeed(
                f"curl -sf {AUTH} {API}/api/disks | "
                f"jq -r 'to_entries[] as $n | $n.value[] | {SPARE} | \"\\($n.key) \\(.id)\"'"
            ).strip().splitlines()
            for line in offered:
                node_name, disk_id = line.split()
                node1.succeed(
                    f"curl -sf -X PUT {AUTH} -H 'content-type: application/json' "
                    f"-d '{{\"desired\":\"ON\"}}' {API}/api/disks/{node_name}/{disk_id}"
                )
            node1.wait_until_succeeds(
                "ceph osd stat --connect-timeout 10 | grep -E '4 osds: 4 up'", timeout=900
            )

        # ── Neither pool replicates: both images and CephFS pools are
        #    created at size=1 regardless of node count (see
        #    homelab/local-api/src/storage/images_rbd.rs and cephfs.rs — read
        #    directly, not assumed). A 2-node cluster has no more redundancy
        #    against a single lost disk than a 1-node one does; the extra
        #    node buys capacity and mesh/API continuity, not data survival.
        #    So this test does NOT expect the cluster to self-heal a lost OSD
        #    — disk-loss-test already establishes that FORCE HEAL is the real
        #    recovery path. What's actually new here, and worth its own VM
        #    test, is whether that same detection holds up once the lost disk
        #    belongs to only ONE of several machines: does the problem get
        #    reported without silently going unnoticed, and does the
        #    machine that was NOT touched stay fully healthy throughout? ────
        sizes_before = node1.succeed("ceph osd pool ls detail -f json")
        node1.succeed(f"""echo '{sizes_before}' | jq -e '[.[] | select(.size != 1)] | length == 0'""")

        # ── Pull a disk from node2 specifically — not node1, the machine
        #    every other test in this file already exercises most heavily —
        #    to catch anything that quietly assumed "the affected machine is
        #    always node1" ───────────────────────────────────────────────
        with step(node2, "node2's spare disk is pulled"):
            osd_meta = node2.succeed(
                "ceph osd metadata $(ceph osd tree -f json | "
                "jq -r '[.nodes[] | select(.type==\"host\" and .name==\"yolab-n2\") | .children[]] "
                "| map(select(. != (\"ceph osd tree -f json\" | \"\") | .)) | .[0]' 2>/dev/null || true)"
            )
            # Simpler and robust: ask Ceph directly which OSD lives on node2
            # and is backed by a real block device (not the loopback system LV).
            osd_id = node2.succeed(
                "ceph osd tree -f json | jq -r "
                "'[.nodes[] | select(.type==\"host\" and .name==\"yolab-n2\")][0].children[0]'"
            ).strip()
            dev = node2.succeed(f"ceph osd metadata {osd_id} -f json | jq -r .devices").strip()
            assert dev.startswith("vd"), dev
            pci = node2.succeed(
                f"basename $(dirname $(readlink -f /sys/block/{dev}/device))"
            ).strip()
            node2.succeed(f"echo -n {pci} > /sys/bus/pci/drivers/virtio-pci/unbind")
            node2.fail(f"test -e /dev/{dev}")
            node1.wait_until_succeeds(
                f"ceph osd tree -f json | jq -e '.nodes[] | select(.id=={osd_id}) | .status==\"down\"'",
                timeout=600,
            )

        with step(node1, "the OTHER node is untouched: mesh, quorum and API all still fine"):
            node1.succeed("ping -6 -c3 fd00:cafe::2")
            node1.succeed("systemctl is-active ceph-mon-yolab-n1.service")
            node1.succeed(
                "ceph -s --connect-timeout 10 | grep -E 'quorum .*yolab-n1.*yolab-n2|quorum .*yolab-n2.*yolab-n1'"
            )
            node1.succeed(f"{K} get --raw /readyz")
            node1.succeed(
                f"{K} wait --for=condition=Ready node --all --timeout=60s"
            )

        with step(node1, "the loss is reported, not silently absorbed"):
            jq_ok(
                node1,
                f"curl -sf {AUTH} {API}/api/heal",
                '.problems | index("data_unreachable") != null',
                600,
            )

        with step(node2, "node2's own k3s agent is unaffected by its lost disk"):
            node2.succeed("systemctl is-active k3s.service")
      '';
  }
