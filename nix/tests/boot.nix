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
  };
in
  pkgs.testers.nixosTest {
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
      '';
  }
