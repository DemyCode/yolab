{pkgs, ...}: {
  tree-remove-tests =
    pkgs.runCommand "tree-remove-tests"
    {
      nativeBuildInputs = [pkgs.busybox];
      src = ../../homelab/local-api/src;
    }
    ''
      busybox sh "$src/tree_remove_test.sh"
      touch $out
    '';

  osd-memory-target-tests =
    pkgs.runCommand "osd-memory-target-tests"
    {
      nativeBuildInputs = [pkgs.busybox];
      src = ../../homelab/nixos/ceph;
    }
    ''
      busybox sh "$src/osd_memory_target_test.sh"
      touch $out
    '';

  wg-register-tests =
    pkgs.runCommand "wg-register-tests"
    {
      nativeBuildInputs = [
        pkgs.busybox
        pkgs.jq
      ];
      src = ../../apps/wg-register;
    }
    ''
      cp -r "$src" ./wg-register
      chmod -R +w ./wg-register
      busybox sh ./wg-register/setup_test.sh
      touch $out
    '';

  wg-sidecar-firewall-tests =
    pkgs.runCommand "wg-sidecar-firewall-tests"
    {
      nativeBuildInputs = [pkgs.busybox];
      src = ../../apps/wg-sidecar;
    }
    ''
      busybox sh "$src/firewall_test.sh"
      touch $out
    '';

  chart-checks =
    pkgs.runCommand "chart-checks"
    {
      nativeBuildInputs = [
        pkgs.kubernetes-helm
        (pkgs.python3.withPackages (ps: [ps.pyyaml]))
      ];
      src = ../../apps/catalog;
    }
    ''
      cp -r "$src" ./catalog
      chmod -R +w ./catalog
      # helm needs a writable home, and the sandbox has none.
      export HOME=$PWD/home
      mkdir -p "$HOME"
      (cd ./catalog && python3 -m unittest check_charts_test)
      python3 ./catalog/check_charts.py
      touch $out
    '';

  update-tests =
    pkgs.runCommand "update-tests"
    {
      nativeBuildInputs = [(pkgs.python3.withPackages (ps: [ps.pyyaml]))];
      src = ../../scripts/update;
    }
    ''
      cp -r "$src" ./update
      (cd ./update && python3 -m unittest update_test)
      touch $out
    '';
  ceph-csi-manifest =
    pkgs.runCommand "ceph-csi-manifest"
    {
      nativeBuildInputs = [(pkgs.python3.withPackages (ps: [ps.pyyaml]))];
      src = ../../homelab/nixos/ceph-csi;
    }
    ''
      cp -r "$src" ./ceph-csi
      (cd ./ceph-csi && python3 -m unittest cephfs_test)
      touch $out
    '';
}
