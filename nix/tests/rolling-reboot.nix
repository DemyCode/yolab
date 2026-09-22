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
      description = "Stub for the WireGuard mesh (rolling-reboot VM test)";
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
    name = "yolab-rolling-reboot";

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
        K = "k3s kubectl"

        def assert_cluster_healthy(step_name):
            with step([node1, node2], f"{step_name}: both mons are up"):
                node1.wait_until_succeeds(
                    "systemctl is-active ceph-mon-yolab-n1.service", timeout=600
                )
                node2.wait_until_succeeds(
                    "systemctl is-active ceph-mon-yolab-n2.service", timeout=900
                )
            with step([node1, node2], f"{step_name}: the mons agree on one quorum"):
                node1.wait_until_succeeds(
                    "ceph -s --connect-timeout 10 | grep -E 'quorum .*yolab-n1.*yolab-n2|quorum .*yolab-n2.*yolab-n1'",
                    timeout=600,
                )
            with step(node1, f"{step_name}: k3s sees both nodes Ready"):
                node1.wait_until_succeeds(f"{K} get --raw /readyz", timeout=900)
                node1.wait_until_succeeds(
                    f"{K} get nodes -o name | wc -l | grep -qx 2", timeout=900
                )
                node1.wait_until_succeeds(
                    f"{K} wait --for=condition=Ready node --all --timeout=60s", timeout=900
                )

        start_all()

        with step([node1, node2], "the mesh comes up"):
            for m in (node1, node2):
                m.wait_for_unit("multi-user.target", timeout=900)
            node1.succeed("ping -6 -c3 fd00:cafe::2")
            node2.succeed("ping -6 -c3 fd00:cafe::1")

        assert_cluster_healthy("before any reboot")

        fsid_before = node1.succeed("ceph fsid --connect-timeout 10").strip()

        # ── Reboot node2 alone, the way a rolling nixos-rebuild / kernel
        #    upgrade actually happens on this platform: one node down at a
        #    time, never both.
        #
        # This does NOT assert node1 keeps serving while node2 is down. With
        # only two nodes in the mesh, both the mon quorum and k3s's embedded
        # etcd need a majority of TWO to read or write, so losing either node
        # stalls the API on the survivor as well — see
        # homelab/nixos/common.nix's own comment that the cluster "is HA once
        # there are 3+" and project_k3s_topology_gap. Asserting continued
        # availability here would just be asserting something false; the
        # property that actually matters at this topology is what this test
        # checks instead: node2 must rejoin the SAME cluster on its own once
        # it is back, not bootstrap a second one — which looks identical to
        # "healthy" on node2 alone and is exactly the failure
        # two-node-test's fsid check exists for. ─────────────────────────────
        with step(node2, "node2 goes down for maintenance"):
            node2.shutdown()

        with step(node1, "node1 stays up as a process, even though the API stalls"):
            node1.succeed("systemctl is-active ceph-mon-yolab-n1.service")
            node1.succeed("systemctl is-active k3s.service")

        with step(node2, "node2 comes back up"):
            node2.start()
            node2.wait_for_unit("multi-user.target", timeout=900)

        assert_cluster_healthy("after node2's reboot")

        with step([node1, node2], "the cluster is still the SAME cluster, not two"):
            fsids = {
                m.succeed("ceph fsid --connect-timeout 10").strip() for m in (node1, node2)
            }
            assert len(fsids) == 1, f"nodes disagree about the cluster fsid: {fsids}"
            assert fsid_before in fsids, (
                f"the cluster's fsid changed across node2's reboot: "
                f"{fsid_before!r} -> {fsids!r} — node2 bootstrapped a fresh "
                "cluster instead of rejoining"
            )

        with step(node2, "node2's k3s agent is active again, not stuck retrying"):
            node2.succeed("systemctl is-active k3s.service")

        # ── Now the other way around: node1 goes down while node2 carries on.
        #    Not symmetric by construction — node1 is the one every other test
        #    in this suite treats as "the" node, so this is the direction most
        #    likely to have an unwritten assumption baked into it ───────────
        with step(node1, "node1 goes down for maintenance"):
            node1.shutdown()

        with step(node2, "node2 stays up as a process, even though the API stalls"):
            node2.succeed("systemctl is-active ceph-mon-yolab-n2.service")
            node2.succeed("systemctl is-active k3s.service")

        with step(node1, "node1 comes back up"):
            node1.start()
            node1.wait_for_unit("multi-user.target", timeout=900)

        assert_cluster_healthy("after node1's reboot")

        with step([node1, node2], "the cluster is still the SAME cluster, not two"):
            fsids = {
                m.succeed("ceph fsid --connect-timeout 10").strip() for m in (node1, node2)
            }
            assert len(fsids) == 1, f"nodes disagree about the cluster fsid: {fsids}"
            assert fsid_before in fsids, (
                f"the cluster's fsid changed across node1's reboot: "
                f"{fsid_before!r} -> {fsids!r}"
            )
      '';
  }
