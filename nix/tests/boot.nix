# Single-node boot test: prove one yolab machine comes up the whole way —
# multi-user, a Ceph mon in quorum with itself, k3s serving, and local-api
# answering.
#
# This is the cheapest test that can fail for a real reason, so it is the one to
# read first when CI goes red: two-node.nix and disk-loss.nix both build on a
# machine that boots, and if this is failing their failures are downstream of it.
#
# WHAT IT DELIBERATELY DOES NOT COVER. No OSD is switched on and no image RBD is
# asserted. Storage that deep needs a second machine to be meaningful — a
# one-copy pool is a topology nobody runs — and two-node.nix already walks it
# end to end. Keeping this one shallow is what keeps it fast enough to stay the
# first thing anyone looks at.
{
  pkgs,
  inputs,
  rust,
  disko,
}: let
  testLib = import ./lib.nix;
  bootConfigPath = ../../homelab/tests/boot-config.toml;
  vmModule = {lib, ...}: {
    # All four of the specialArgs the real `nixosSystem` passes (see flake.nix).
    #
    # `localApiEnv` is the one that gets forgotten, and forgetting it is not a
    # runtime failure — nothing references it until a module deep in common.nix
    # builds a unit's ExecStart from it, and the eval then dies with "attribute
    # 'localApiEnv' missing" raised from inside nixpkgs' module system, naming
    # nothing in this file. This test shipped without it and could never have
    # passed; it was simply never run. That is what the `vm` job in
    # .github/workflows/push.yml exists to stop.
    _module.args = {
      inherit inputs rust;
      yolabConfigPath = bootConfigPath;
      localApiEnv = rust.crates.local-api.package;
    };
    imports = [
      disko.nixosModules.disko
      ../../homelab/nixos/configuration.nix
      ../../homelab/nixos/disk-config.nix
    ];

    # The VM boots the harness's own root image, so the install-time LVM layout
    # is neutralised and GRUB pointed at the virtual disk.
    disko.devices = lib.mkForce {};
    boot.loader.grub.enable = lib.mkForce true;
    boot.loader.grub.device = lib.mkForce "/dev/vda";
    boot.loader.grub.efiSupport = lib.mkForce false;

    # A Ceph mon plus a k3s server plus containerd is not a small machine. The
    # 2048MB this used to ask for is under what k3s alone takes on a live node,
    # and an OOM in here reads as a mysterious hang rather than a failure.
    virtualisation.memorySize = 4096;
    virtualisation.cores = 2;
    # A spare disk, so the machine looks like one with storage to offer even
    # though this test never switches it on. Its absence changes what the disk
    # reconciler reports, and "boots with a spare disk present" is the shape
    # every real machine has.
    virtualisation.emptyDiskImages = [8192];

    # ── The mesh, without WireGuard ──────────────────────────────────────────
    #
    # Same substitution two-node.nix makes and for the same reason: there is no
    # WireGuard server in this test. mkForce {} removes the interface definition
    # and the unit that would dial out; the address it would have configured goes
    # on the test LAN with the same prefix length, so fd00:cafe::/112 is on-link
    # exactly as common.nix's postSetup route arranges in production.
    networking.wireguard.interfaces = lib.mkForce {};
    networking.interfaces.eth1.ipv6.addresses = [
      {
        address = "fd00:cafe::1";
        prefixLength = 112;
      }
    ];
    # Several units are ordered `after` this one. Ordering against a unit that
    # does not exist is silently a no-op, which would let them race the address
    # being configured, so the stub genuinely completes once the mesh is up.
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
