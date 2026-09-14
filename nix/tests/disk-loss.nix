# Disk-loss VM test: pull a disk out of a one-copy cluster and watch it heal.
#
# One machine, two OSD disks, every pool at one copy — so each placement group
# lives on exactly one disk and hot-unplugging either loses data for real. The
# test then asserts, without any operator action, that storage_heal:
#
#   * declares the unplugged OSD lost once its disk has been absent past the
#     (shortened) grace period,
#   * rebuilds the lost placement groups and the CephFS filesystem,
#   * replaces the stand-in app's volume with a fresh claim and scales the app
#     back to its original replicas,
#   * marks that app corrupted, both in its ConfigMap and on the API the home
#     page reads,
#   * purges the lost OSD, leaving every placement group active.
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

    # A minute instead of fifteen: long enough to prove the grace period is
    # honoured, short enough for a test.
    systemd.services.yolab-local-api.environment.YOLAB_STORAGE_HEAL_DISK_GRACE_SECS = "60";
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
              "journalctl -u yolab-local-api --no-pager | grep -E 'storage-heal|disk' | tail -80"
          )[1])
          print(node1.execute("ceph -s; ceph osd tree; ceph fs ls")[1])
          print(node1.execute(f"{K} get cm yolab-storage-heal -n kube-system -o yaml")[1])

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

      # Inside the grace period nothing may have been declared.
      node1.sleep(20)
      node1.succeed("ceph osd dump -f json | jq -e '.osds[] | select(.osd==1) | .lost_at == 0'")

      # ── It heals on its own ───────────────────────────────────────────────
      try:
          jq_ok("ceph osd ls -f json", "index(1) == null", 2400)
          jq_ok("ceph pg dump pgs_brief -f json",
                '[.pg_stats[] | select(.state | test("active") | not)] | length == 0', 900)
          jq_ok(f"{K} get cm yolab-storage-heal -n kube-system -o json",
                '.data.state | fromjson | .rebuild == null', 900)
      except Exception:
          dump_heal_log()
          raise

      jq_ok("ceph fs ls -f json", 'map(.name) | index("yolab-fs") != null', 60)
      node1.wait_until_succeeds("ceph mds stat | grep -q 'up:active'", timeout=600)
      node1.succeed("ceph fs subvolumegroup ls yolab-fs | grep -q csi")

      new_uid = node1.succeed(f"{K} get pvc data -n yolab-demo -o jsonpath='{{.metadata.uid}}'").strip()
      assert new_uid and new_uid != old_uid, f"the claim was not replaced ({old_uid} -> {new_uid})"
      node1.succeed(f"{K} get deploy web -n yolab-demo -o jsonpath='{{.spec.replicas}}' | grep -qx 2")

      jq_ok(f"{K} get cm yolab-data-loss -n kube-system -o json",
            '.data.apps | fromjson | index("yolab-demo") != null', 60)
      jq_ok(f"curl -sf {AUTH} {API}/api/backups/damage",
            '.apps | map(.namespace) | index("yolab-demo") != null', 120)

      node1.succeed("ceph osd stat | grep -E '1 osds: 1 up'")
    '';
  }
