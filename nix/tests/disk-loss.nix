# Disk-loss VM test: pull a disk out of a one-copy cluster and FORCE HEAL it.
#
# One machine, two OSD disks, every pool at one copy — so each placement group
# lives on exactly one disk and hot-unplugging either loses data for real. The
# test asserts that nothing happens on its own, and then the two halves of a
# FORCE HEAL (homelab/local-api/src/heal/) a VM can run:
#
#   * a heal whose `nixos-rebuild boot` fails is undone, and leaves the machine
#     exactly as it was — the VM has no flake repo to rebuild from, which is as
#     real a failure as any;
#   * `[node] wipe_condition = true` makes the next boot (storage/reset_wipe.rs)
#     turn the machine into a fresh one: every OSD erased, no disk switched
#     on, no apps, the flag cleared, and the cluster created again.
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
    # A writable machine directory, as on a real machine: a heal rewrites
    # config.toml, and the wipe clears its flag.
    systemd.tmpfiles.rules = ["C /var/lib/yolab/machine/config.toml 0600 root root - ${configPath}"];
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
              "journalctl -u yolab-local-api -u yolab-reset-wipe --no-pager | grep -E 'heal|disk|reset' | tail -120"
          )[1])
          print(node1.execute("ceph -s; ceph osd tree; ceph fs ls")[1])
          print(node1.execute("cat /var/lib/yolab/heal.json /var/lib/yolab/reset/state.json")[1])

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

      # ── FORCE HEAL that cannot build: undone ──────────────────────────────
      jq_ok(f"curl -sf {AUTH} {API}/api/heal",
            '(.problems | index("data_unreachable") != null) and .refusal == null '
            'and .plan.keep_machines == ["yolab-n1"] and .plan.remove_machines == []', 600)
      node1.succeed(
          f"curl -sf -X POST {AUTH} -H 'content-type: application/json' "
          f"-d '{{\"keep_machines\":[\"yolab-n1\"],\"remove_machines\":[]}}' {API}/api/heal"
      )
      try:
          jq_ok(f"curl -sf {AUTH} {API}/api/heal",
                '.heal.running == false and (.heal.failed | test("could not prepare"))', 900)
      except Exception:
          dump_heal_log()
          raise
      MACHINE_CONFIG = "/var/lib/yolab/machine/config.toml"
      node1.fail(f"grep -q wipe_condition.*true {MACHINE_CONFIG}")
      node1.fail("test -e /var/lib/yolab/reset/config.toml.before")
      node1.succeed("ceph osd ls -f json | jq -e 'index(1) != null'")
      node1.succeed(f"{K} get namespace yolab-demo")
      node1.succeed(f"grep -q 11111111-2222-3333-4444-555555555555 {MACHINE_CONFIG}")

      # ── The wipe at boot: a fresh machine ─────────────────────────────────
      node1.succeed(f"sed -i 's/^\\[node\\]$/[node]\\nwipe_condition = true/' {MACHINE_CONFIG}")
      node1.succeed(f"grep -q '^wipe_condition = true' {MACHINE_CONFIG}")
      node1.shutdown()
      node1.start()
      node1.wait_for_unit("multi-user.target", timeout=900)
      try:
          node1.wait_until_succeeds("systemctl show -p Result yolab-reset-wipe | grep -q success", timeout=300)
          node1.succeed(f"grep -q 'wipe_condition = false' {MACHINE_CONFIG}")
          node1.wait_until_succeeds("systemctl is-active ceph-mon-yolab-n1.service", timeout=600)
          node1.wait_until_succeeds(f"{K} get --raw /readyz", timeout=900)
      except Exception:
          dump_heal_log()
          raise
      node1.succeed(f"{K} create namespace rook-ceph --dry-run=client -o yaml | {K} apply -f -")
      node1.succeed("ceph osd ls -f json | jq -e 'length == 0'")
      node1.fail(f"{K} get namespace yolab-demo")
      node1.wait_for_open_port(3001, timeout=300)
      # Both disks are blank again, and nothing switches them on.
      jq_ok(f"curl -sf {AUTH} {API}/api/disks", f"[.[][] | {SPARE}] | length == 2", 600)
      jq_ok("ceph config-key dump yolab/disks/", 'to_entries | map(select(.value == "OFF")) | length == 2', 600)
      # The k3s manifests tmpfiles links in survive the wipe.
      node1.succeed("test -e /var/lib/rancher/k3s/server/manifests/rook-ceph-operator.yaml")
    '';
  }
