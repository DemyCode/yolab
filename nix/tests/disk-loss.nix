# Disk-loss VM test: pull a disk out of a one-copy cluster and FORCE HEAL it.
#
# One machine, two OSD disks, every pool at one copy — so each placement group
# lives on exactly one disk and hot-unplugging either loses data for real. The
# test asserts that nothing happens on its own, and that FORCE HEAL (heal.rs):
#
#   * reports the unreachable data and plans to remove no machine,
#   * purges the unplugged OSD and switches its disk OFF,
#   * deletes every pool and the stand-in app, restarts the machine,
#   * comes back with the app filesystem recreated, the image store recreated,
#     every placement group active, and the heal recorded as finished.
#
# The unplug is done from inside the guest by unbinding the disk's virtio PCI
# device, which removes the block device the way a yanked USB cable does. The
# CSI driver, VolSync and the app image are not in this test (no internet): the
# claim stays Pending and the pods never pull, which is enough — what is under
# test is what happens to Ceph and to the Kubernetes objects.
{
  pkgs,
  inputs,
  rust,
  disko,
}: let
  configPath = ../../homelab/tests/boot-config.toml;
  meshAddr = "fd00:cafe::1";

  node = {lib, ...}: {
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

    virtualisation.memorySize = 6144;
    virtualisation.cores = 4;
    virtualisation.emptyDiskImages = [8192 8192];

    # Same stand-in for the mesh as two-node.nix; see the note there.
    networking.wireguard.interfaces = lib.mkForce {};
    networking.interfaces.eth1.ipv6.addresses = [
      {
        address = meshAddr;
        prefixLength = 112;
      }
    ];
    systemd.services.wireguard-wg1 = {
      description = "Stub for the WireGuard mesh (disk-loss VM test)";
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
          exit 1
        '';
      };
    };
    networking.useDHCP = lib.mkDefault false;
    environment.systemPackages = [pkgs.curl pkgs.jq];
    environment.etc."nixos/homelab/ignored/config.toml".source = configPath;
  };
in
  pkgs.testers.nixosTest {
    name = "yolab-disk-loss";
    nodes.node1 = node;

    testScript = ''
      import json

      TOKEN = "test-account-token"
      AUTH = f"-H 'x-yolab-cluster: {TOKEN}'"
      API = "http://[::1]:3001"
      K = "k3s kubectl"

      def jq_ok(cmd, expr, timeout):
          node1.wait_until_succeeds(f"{cmd} | jq -e '{expr}' >/dev/null", timeout=timeout)

      def dump_heal_log():
          print(node1.execute(
              "journalctl -u yolab-local-api --no-pager | grep -E 'heal|disk' | tail -120"
          )[1])
          print(node1.execute("ceph -s; ceph osd tree; ceph fs ls")[1])
          print(node1.execute("cat /var/lib/yolab/heal.json")[1])

      start_all()
      node1.wait_for_unit("multi-user.target", timeout=900)
      node1.wait_until_succeeds("systemctl is-active ceph-mon-yolab-n1.service", timeout=600)
      node1.wait_until_succeeds(f"{K} get --raw /readyz", timeout=900)
      node1.succeed(f"{K} create namespace rook-ceph --dry-run=client -o yaml | {K} apply -f -")

      # ── Two disks on, one copy of everything ──────────────────────────────
      node1.wait_for_open_port(3001, timeout=300)
      SPARE = "select(.is_our_osd==false and .has_partitions==false and .mounted==false)"
      jq_ok(f"curl -sf {AUTH} {API}/api/disks", f"[.[][] | {SPARE}] | length == 2", 600)
      for line in node1.succeed(
          f"curl -sf {AUTH} {API}/api/disks | "
          f"jq -r 'to_entries[] as $n | $n.value[] | {SPARE} | \"\\($n.key) \\(.id)\"'"
      ).strip().splitlines():
          node_name, disk_id = line.split()
          node1.succeed(
              f"curl -sf -X PUT {AUTH} -H 'content-type: application/json' "
              f"-d '{{\"desired\":\"ON\"}}' {API}/api/disks/{node_name}/{disk_id}"
          )
      node1.wait_until_succeeds("ceph osd stat | grep -E '2 osds: 2 up'", timeout=900)

      # CephFS appears once an OSD exists (cephfs.rs), with an active MDS.
      jq_ok("ceph fs ls -f json", 'map(.name) | index("yolab-fs") != null', 600)
      node1.wait_until_succeeds("ceph mds stat | grep -q 'up:active'", timeout=600)
      node1.wait_until_succeeds("ceph fs subvolumegroup ls yolab-fs | grep -q csi", timeout=600)
      node1.succeed("ceph fs subvolume create yolab-fs demo --group_name csi")

      sizes = json.loads(node1.succeed("ceph osd pool ls detail -f json"))
      assert all(p["size"] == 1 for p in sizes), [(p["pool_name"], p["size"]) for p in sizes]
      jq_ok("ceph pg dump pgs_brief -f json",
            '[.pg_stats[] | select(.state | test("active") | not)] | length == 0', 900)

      # ── A stand-in app with a volume on CephFS ────────────────────────────
      node1.succeed(f"{K} create namespace yolab-demo")
      node1.succeed(f"{K} label namespace yolab-demo yolab.io/managed=true")
      node1.succeed(f"""{K} apply -f - <<'EOF'
      apiVersion: v1
      kind: PersistentVolumeClaim
      metadata: {{name: data, namespace: yolab-demo}}
      spec:
        accessModes: [ReadWriteMany]
        storageClassName: yolab-cephfs
        resources: {{requests: {{storage: 1Gi}}}}
      EOF""")
      node1.succeed(f"{K} create deployment web --image=busybox --replicas=2 -n yolab-demo")
      old_uid = node1.succeed(f"{K} get pvc data -n yolab-demo -o jsonpath='{{.metadata.uid}}'").strip()

      # ── Pull osd.1's disk ─────────────────────────────────────────────────
      dev = node1.succeed("ceph osd metadata 1 -f json | jq -r .devices").strip()
      assert dev.startswith("vd"), dev
      pci = node1.succeed(f"basename $(dirname $(readlink -f /sys/block/{dev}/device))").strip()
      node1.succeed(f"echo -n {pci} > /sys/bus/pci/drivers/virtio-pci/unbind")
      node1.fail(f"test -e /dev/{dev}")
      node1.wait_until_succeeds("ceph osd tree -f json | jq -e '.nodes[] | select(.id==1) | .status==\"down\"'", timeout=600)

      # ── Nothing heals on its own ──────────────────────────────────────────
      node1.sleep(60)
      node1.succeed("ceph osd ls -f json | jq -e 'index(1) != null'")
      node1.succeed(f"{K} get namespace yolab-demo")

      # ── FORCE HEAL ────────────────────────────────────────────────────────
      jq_ok(f"curl -sf {AUTH} {API}/api/heal",
            '(.problems | index("data_unreachable") != null) and .refusal == null '
            'and .plan.remove_machines == [] and .plan.reset_kubernetes == false', 600)
      node1.succeed(
          f"curl -sf -X POST {AUTH} -H 'content-type: application/json' "
          f"-d '{{\"remove_machines\":[]}}' {API}/api/heal"
      )
      try:
          # The heal restarts the machine last; the test driver sees that as a
          # shutdown and starts it again.
          node1.wait_for_shutdown()
      except Exception:
          dump_heal_log()
          raise
      node1.start()
      node1.wait_for_unit("multi-user.target", timeout=900)
      node1.wait_for_open_port(3001, timeout=600)

      try:
          jq_ok(f"curl -sf {AUTH} {API}/api/heal", '.heal.running == false', 1800)
      except Exception:
          dump_heal_log()
          raise

      node1.succeed("ceph osd ls -f json | jq -e 'index(1) == null'")
      node1.fail(f"{K} get namespace yolab-demo")
      jq_ok("ceph config-key dump yolab/disks/", 'to_entries | map(select(.value == "OFF")) | length == 1', 60)
      jq_ok("ceph fs ls -f json", 'map(.name) | index("yolab-fs") != null', 900)
      node1.wait_until_succeeds("ceph mds stat | grep -q 'up:active'", timeout=600)
      node1.wait_until_succeeds("ceph fs subvolumegroup ls yolab-fs | grep -q csi", timeout=600)
      node1.succeed("ceph osd pool ls | grep -qx images")
      node1.succeed("findmnt /var/lib/rancher/k3s/agent/containerd")
      jq_ok("ceph pg dump pgs_brief -f json",
            '[.pg_stats[] | select(.state | test("active") | not)] | length == 0', 900)
      node1.succeed("ceph osd stat | grep -E '1 osds: 1 up'")
    '';
  }
