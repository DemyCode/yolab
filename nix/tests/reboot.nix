{
  pkgs,
  inputs,
  disko,
  yolabSpecialArgs,
}: let
  testLib = import ./lib.nix {inherit pkgs;};
  configPath = ../../homelab/tests/boot-config.toml;
  meshAddr = "fd00:cafe::1";

  node = {lib, ...}: {
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
    # The default root disk ("auto"-sized to the system closure, no slack)
    # leaves swapspace (homelab/nixos/common.nix's services.swapspace) no
    # room to ever create a swapfile at /var/lib/swapspace, so real memory
    # pressure goes straight to the OOM killer instead of being absorbed by
    # swap. Room for a few GB of swap on top of the closure.
    virtualisation.diskSize = 8192;

    networking.wireguard.interfaces = lib.mkForce {};
    networking.interfaces.eth1.ipv6.addresses = [
      {
        address = meshAddr;
        prefixLength = 112;
      }
    ];
    systemd.services.wireguard-wg1 = {
      description = "Stub for the WireGuard mesh (reboot VM test)";
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
  };
in
  testLib.withNetwork (pkgs.testers.nixosTest {
    name = "yolab-reboot";
    nodes.node1 = node;

    testScript =
      testLib.preamble
      + ''
        TOKEN = "test-account-token"
        AUTH = f"-H 'x-yolab-cluster: {TOKEN}'"
        API = "http://[::1]:3001"
        K = "k3s kubectl"

        def jq_ok(cmd, expr, timeout):
            node1.wait_until_succeeds(f"{cmd} | jq -e '{expr}' >/dev/null", timeout=timeout)

        def assert_stack_healthy(step_name):
            """The full boot line, from mon quorum to the API answering — the
            same properties boot.nix checks on a cold boot, reasserted here
            after a warm reboot. Nothing here should behave differently the
            second time: a unit that only works because it happened to start
            in an empty, freshly-formatted world is exactly the bug this test
            exists to catch."""
            with step(node1, f"{step_name}: the mon comes up"):
                node1.wait_until_succeeds(
                    "systemctl is-active ceph-mon-yolab-n1.service", timeout=600
                )
            with step(node1, f"{step_name}: the mon reaches quorum with itself"):
                node1.wait_until_succeeds(
                    "ceph -s --connect-timeout 10 | grep -E 'quorum .*yolab-n1'",
                    timeout=600,
                )
            with step(node1, f"{step_name}: k3s serves its API"):
                node1.wait_until_succeeds(f"{K} get --raw /readyz", timeout=900)
            with step(node1, f"{step_name}: the node reports Ready"):
                node1.wait_until_succeeds(
                    f"{K} wait --for=condition=Ready node --all --timeout=60s",
                    timeout=900,
                )
            with step(node1, f"{step_name}: local-api is listening and answering"):
                node1.wait_for_open_port(3001, timeout=300)
                node1.wait_until_succeeds(f"curl -sf {AUTH} {API}/api/disks", timeout=300)

        start_all()

        with step(node1, "first boot reaches multi-user.target"):
            node1.wait_for_unit("multi-user.target", timeout=900)
        assert_stack_healthy("first boot")

        # ── services.swapspace exists so real memory pressure gets absorbed
        #    instead of going straight to the OOM killer — but the VM's root
        #    disk (where it creates swapfiles, /var/lib/swapspace) is
        #    "auto"-sized to the system closure with zero slack by default,
        #    which left swapspace unable to ever allocate anything and let a
        #    2026-09-22 CI run OOM-kill coredns mid-test. virtualisation.diskSize
        #    above fixes the room; this proves swap actually engages, not
        #    just that the daemon starts ──────────────────────────────────
        with step(node1, "first boot: swap actually has room to engage under pressure"):
            total_mb = int(node1.succeed("free -m | awk '/^Mem:/{print $2}'").strip())
            fill_mb = total_mb * 9 // 10
            try:
                node1.succeed(f"dd if=/dev/zero of=/dev/shm/pressure bs=1M count={fill_mb} status=none")
                node1.wait_until_succeeds(
                    '[ "$(wc -l < /proc/swaps)" -gt 1 ]', timeout=120
                )
            finally:
                node1.succeed("rm -f /dev/shm/pressure")

        node1.succeed(
            f"{K} create namespace rook-ceph --dry-run=client -o yaml | {K} apply -f -"
        )

        # ── Switch on the spare disk, so the reboot has real storage state to
        #    lose: an OSD, a pool, and containerd's data-root on the RBD ──────
        SPARE = "select(.is_our_osd==false and .has_partitions==false and .mounted==false)"
        jq_ok(f"curl -sf {AUTH} {API}/api/disks", f"[.[][] | {SPARE}] | length == 1", 600)
        disk_id = node1.succeed(
            f"curl -sf {AUTH} {API}/api/disks | "
            f"jq -r '.[\"yolab-n1\"][] | {SPARE} | .id' | head -1"
        ).strip()
        node1.succeed(
            f"curl -sf -X PUT {AUTH} -H 'content-type: application/json' "
            f"-d '{{\"desired\":\"ON\"}}' {API}/api/disks/yolab-n1/{disk_id}"
        )
        with step(node1, "the spare disk becomes a second, up OSD"):
            node1.wait_until_succeeds(
                "ceph osd stat --connect-timeout 10 | grep -E '2 osds: 2 up'", timeout=900
            )
        with step(node1, "containerd's store moves onto the RBD"):
            node1.wait_until_succeeds(
                "findmnt -rno SOURCE --mountpoint /var/lib/rancher/k3s/agent/containerd "
                "| grep -q '^/dev/rbd'",
                timeout=900,
            )

        # ── A stand-in app with a volume, so the reboot has to bring back
        #    something a person would actually lose if this broke ───────────
        #
        # Not waiting for the PVC to bind or the deployment to go Ready: the
        # VM test sandbox has no internet, so the Rook operator's Helm chart
        # (fetched from https://charts.rook.io) never installs and there is no
        # CephFS CSI driver to actually provision a volume — the same
        # constraint two-node-test's rook-ceph-namespace comment describes.
        # disk-loss-test hits the same wall and works around it the same way:
        # create the objects, track the PVC's UID, and use survival of that
        # UID across the reboot as the assertion, rather than a bind that
        # cannot happen in this environment.
        #
        # The deployment does NOT mount that PVC — it never depended on the
        # CSI driver to begin with — so its pods can and do reach Ready, once
        # they reference an image this sandbox actually has: one Nix built
        # locally and imported into containerd, not one docker.io was asked
        # to pull. See demoImage's comment in lib.nix.
        node1.succeed(f"{K} create namespace yolab-demo")
        node1.succeed(f"{K} label namespace yolab-demo yolab.io/managed=true")
        node1.succeed("k3s ctr -n k8s.io images import ${testLib.demoImage}")
        node1.succeed(f"""{K} apply -f - <<'EOF'
        apiVersion: v1
        kind: PersistentVolumeClaim
        metadata: {{name: data, namespace: yolab-demo}}
        spec:
          accessModes: [ReadWriteMany]
          storageClassName: yolab-cephfs
          resources: {{requests: {{storage: 1Gi}}}}
        EOF""")
        node1.succeed(f"""{K} apply -f - <<'EOF'
        apiVersion: apps/v1
        kind: Deployment
        metadata: {{name: web, namespace: yolab-demo}}
        spec:
          replicas: 1
          selector: {{matchLabels: {{app: web}}}}
          template:
            metadata: {{labels: {{app: web}}}}
            spec:
              containers:
              - name: web
                image: ${testLib.demoImageName}:${testLib.demoImageTag}
                imagePullPolicy: Never
        EOF""")
        pvc_uid_before = node1.succeed(
            f"{K} get pvc data -n yolab-demo -o jsonpath='{{.metadata.uid}}'"
        ).strip()
        with step(node1, "the demo app reaches Ready before the reboot"):
            jq_ok(f"{K} get deployment web -n yolab-demo -o json", ".status.readyReplicas == 1", 300)

        # ── The actual test: a warm reboot, nothing else changed ────────────
        #
        # Not the disk-loss.nix reboot: there `wipe_condition` is set first and
        # a clean slate is the *point*. Here nothing is broken and nothing is
        # armed to wipe — this is the ordinary "the box lost power" case, which
        # is the single most common real-world event a homelab node sees and,
        # until this test, was only ever exercised as a cold boot into an empty
        # disk image.
        with step(node1, "a warm reboot"):
            node1.shutdown()
            node1.start()
            node1.wait_for_unit("multi-user.target", timeout=900)

        assert_stack_healthy("after reboot")

        with step(node1, "after reboot: both OSDs come back up, not just one"):
            node1.wait_until_succeeds(
                "ceph osd stat --connect-timeout 10 | grep -E '2 osds: 2 up'", timeout=900
            )

        with step(node1, "after reboot: containerd's store is still the RBD"):
            node1.wait_until_succeeds(
                "findmnt -rno SOURCE --mountpoint /var/lib/rancher/k3s/agent/containerd "
                "| grep -q '^/dev/rbd'",
                timeout=900,
            )
            node1.succeed("ls /var/lib/rancher/k3s/agent/containerd/ >/dev/null")

        with step(node1, "after reboot: k3s is active, not stuck mid-migration"):
            node1.succeed("systemctl is-active k3s.service")

        with step(node1, "after reboot: the demo app and its PVC survived"):
            node1.succeed(f"{K} get namespace yolab-demo")
            jq_ok(f"{K} get deployment web -n yolab-demo -o json", ".status.readyReplicas == 1", 300)
            pvc_uid_after = node1.succeed(
                f"{K} get pvc data -n yolab-demo -o jsonpath='{{.metadata.uid}}'"
            ).strip()
            assert pvc_uid_before == pvc_uid_after, (
                f"PVC was recreated across the reboot instead of surviving: "
                f"{pvc_uid_before!r} -> {pvc_uid_after!r}"
            )

        # ── The other half of the timer-rearm bug: it has to survive a real
        #    reboot, not just a fresh boot where every timer is armed for the
        #    first time and nothing has run yet to fail to re-arm ───────────
        with step(node1, "after reboot: self-healing timers are armed for next time"):
            for unit in (
                "yolab-containerd-store",
                "yolab-ceph-mgr-key",
                "yolab-ceph-mds-key",
                "yolab-images-rbd",
            ):
                nxt = node1.succeed(
                    f"systemctl show {unit}.timer -p NextElapseUSecRealtime "
                    "-p NextElapseUSecMonotonic --value"
                ).split()
                assert any(v not in ("", "infinity") for v in nxt), (
                    f"{unit}.timer will never fire again after a reboot "
                    f"(next elapse: {nxt!r})"
                )
      '';
  })
