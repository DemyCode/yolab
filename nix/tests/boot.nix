{
  pkgs,
  inputs,
  disko,
  yolabSpecialArgs,
}: let
  testLib = import ./lib.nix {inherit pkgs;};
  bootConfigPath = ../../homelab/tests/boot-config.toml;
  vmModule = {lib, ...}: {
    _module.args = yolabSpecialArgs bootConfigPath // {inherit inputs;};
    imports = [
      disko.nixosModules.disko
      ../../homelab/nixos/configuration.nix
      ../../homelab/nixos/disk-config.nix
      (testLib.machine {configPath = bootConfigPath;})
    ];

    disko.devices = lib.mkForce {};
    boot.loader.grub.enable = lib.mkForce true;
    boot.loader.grub.device = lib.mkForce "/dev/vda";
    boot.loader.grub.efiSupport = lib.mkForce false;

    virtualisation.memorySize = 4096;
    virtualisation.cores = 2;
    virtualisation.emptyDiskImages = [8192 8192];
    virtualisation.diskSize = 8192;

    networking.wireguard.interfaces = lib.mkForce {};
    networking.interfaces.eth1.ipv6.addresses = [
      {
        address = "fd00:cafe::1";
        prefixLength = 112;
      }
    ];
    systemd.services.wireguard-wg1 = {
      description = "Stub for the WireGuard mesh (boot VM test)";
      wantedBy = ["multi-user.target"];
      after = ["network-online.target"];
      wants = ["network-online.target"];
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
        ExecStart = "${pkgs.coreutils}/bin/true";
      };
    };

    systemd.services.k3s-node-ip.environment = {
      YOLAB_NODE_IP_MAX_ATTEMPTS = "2";
      YOLAB_NODE_IP_RETRY_DELAY_SECS = "1";
    };
  };
in
  testLib.withNetwork (pkgs.testers.nixosTest {
    name = "yolab-boot";
    nodes.node1 = vmModule;
    testScript =
      testLib.preamble
      + ''

        start_all()

        with step(node1, "the machine reaches multi-user.target"):
            node1.wait_for_unit("multi-user.target", timeout=900)

        # wait_until_succeeds, not wait_for_unit: the mon is `requiredBy` a
        # bootstrap unit that may legitimately fail and retry, and wait_for_unit
        # calls the inactive gap between attempts a permanent failure.
        with step(node1, "the Ceph mon comes up"):
            node1.wait_until_succeeds(
                "systemctl is-active ceph-mon-yolab-n1.service", timeout=600
            )

        with step(node1, "the mon reaches quorum with itself"):
            node1.wait_until_succeeds(
                "ceph -s --connect-timeout 10 | grep -E 'quorum .*yolab-n1'",
                timeout=600,
            )

        # k3s must come up whether or not there is an OSD yet. The image store
        # falls back to the root disk when the RBD is not there, and a k3s that
        # waited for storage instead would leave the machine with nothing able to
        # report why — which is the whole reason yolab-local-api is not
        # After=k3s (see containerd-store-after-order in nix/checks.nix).
        with step(node1, "k3s serves its API"):
            node1.wait_until_succeeds("k3s kubectl get --raw /readyz", timeout=900)

        with step(node1, "the node reports Ready"):
            node1.wait_until_succeeds(
                "k3s kubectl wait --for=condition=Ready node --all --timeout=60s",
                timeout=900,
            )

        # The daemon every page in the UI talks to. Answering at all is the
        # assertion — what it answers with is surface.rs's job, not a VM's.
        with step(node1, "local-api is listening and answering"):
            node1.wait_for_open_port(3001, timeout=300)
            node1.wait_until_succeeds(
                "curl -sf -H 'x-yolab-cluster: test-account-token' "
                "http://[::1]:3001/api/disks",
                timeout=300,
            )

        with step(node1, "the node-ip config k3s actually started with is dual-stack"):
            good_config = node1.succeed("cat /etc/rancher/k3s/config.yaml")
            assert "fd00:cafe::1" in good_config
            assert "," in good_config

        with step(node1, "losing the IPv4 route makes k3s-node-ip refuse, not write a broken config"):
            node1.succeed("ip route del default 2>/dev/null; true")
            node1.fail("systemctl restart k3s-node-ip.service")
            node1.succeed("systemctl is-failed k3s-node-ip.service")
            journal = node1.succeed(
                "journalctl -u k3s-node-ip.service --no-pager -n 50"
            )
            assert "dual-stack cluster-cidr" in journal
            assert node1.succeed("cat /etc/rancher/k3s/config.yaml") == good_config

        with step(node1, "k3s keeps serving on the last known-good config, not crash-looping"):
            node1.succeed("systemctl restart k3s.service")
            node1.wait_until_succeeds("k3s kubectl get --raw /readyz", timeout=300)
            restarts = node1.succeed(
                "systemctl show -p NRestarts --value k3s.service"
            ).strip()
            assert restarts == "0", f"k3s restarted {restarts} times, expected a clean start"
      '';
  })
