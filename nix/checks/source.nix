{
  pkgs,
  treeSrc,
  nixosSystems,
  failOn,
  ...
}: {
  vm-tests-share-one-mesh-stub = let
    dir = ../tests;
    tests = builtins.filter (n: n != "lib.nix" && pkgs.lib.hasSuffix ".nix" n) (builtins.attrNames (builtins.readDir dir));
    copies = builtins.filter (n: pkgs.lib.hasInfix "wireguard-wg1" (builtins.readFile (dir + "/${n}"))) tests;
  in
    failOn "vm-tests-share-one-mesh-stub" {
      problems = map (n: "nix/tests/${n} defines its own wireguard-wg1 stub") copies;
      explain = ''
        Every VM test stubs the WireGuard mesh with testLib.mesh from
        nix/tests/lib.nix, so a fix to the stub reaches all of them:'';
    };

  binary-contract = let
    allBins = pkgs.symlinkJoin {
      name = "yolab-all-unit-bins";
      paths =
        nixosSystems.yolab-ci.config.environment.systemPackages
        ++ pkgs.lib.concatMap (u: u.path or []) (
          builtins.attrValues nixosSystems.yolab-ci.config.systemd.services
        );
    };
  in
    pkgs.runCommand "binary-contract" {nativeBuildInputs = [pkgs.gnugrep];} ''
      grep -rhoE 'Command::new\("[^"]+"\)|\.run_cmd\(\s*"[^"]+"' \
        ${treeSrc}/homelab/local-api/src \
        | grep -oE '"[^"]+"' | tr -d '"' | sort -u > "$TMPDIR/needed.txt"

      missing=""
      while read -r bin; do
        [ -e "${allBins}/bin/$bin" ] || missing="$missing $bin"
      done < "$TMPDIR/needed.txt"

      if [ -n "$missing" ]; then
        echo "local-api shells out to these binaries, but no yolab-owned" >&2
        echo "systemd unit's path (or environment.systemPackages) provides" >&2
        echo "them:$missing" >&2
        exit 1
      fi
      touch $out
    '';
  route-table-is-complete =
    pkgs.runCommand "route-table-is-complete"
    {
      nativeBuildInputs = [
        pkgs.gnugrep
        pkgs.diffutils
      ];
    }
    ''
      src=${treeSrc}/homelab/local-api/src

      # Every path build_router registers. Flattened first: eleven of these are
      # written across several lines, and the obvious grep — one that assumes
      # `.route("` is contiguous — silently finds 58 of the 69 and then reports
      # a clean diff against a table missing the same eleven.
      tr '\n' ' ' < "$src/router.rs" \
        | grep -oE '\.route\([[:space:]]*"[^"]+"' \
        | sed 's/^\.route([[:space:]]*"//; s/"$//' \
        | LC_ALL=C sort -u > registered

      # Every path the table claims. Flattened too: rustfmt splits an entry
      # whose path is long over three lines.
      tr '\n' ' ' < "$src/surface.rs" \
        | grep -oE '\([[:space:]]*"/[^"]*",[[:space:]]*&\[' \
        | sed 's/^([[:space:]]*"//; s/",.*$//' \
        | LC_ALL=C sort -u > tabled

      if ! diff -u tabled registered > delta; then
        echo "homelab/local-api/src/surface.rs ROUTE_TABLE no longer matches the" >&2
        echo "routes registered in homelab/local-api/src/router.rs." >&2
        echo "" >&2
        echo "  '+' lines: registered but NOT in the table — no test sweeps these," >&2
        echo "             including the one that checks they are behind auth." >&2
        echo "  '-' lines: in the table but registered nowhere — a test walking" >&2
        echo "             over a route that does not exist." >&2
        echo "" >&2
        cat delta >&2
        exit 1
      fi
      touch $out
    '';

  custom-services-use-the-library-this-tree-ships =
    pkgs.runCommand "custom-services-use-the-library-this-tree-ships"
    {
      nativeBuildInputs = [pkgs.gnused];
    }
    ''
      pinned=$(sed -n 's/^const CUSTOM_LIBRARY_VERSION: &str = "\(.*\)";$/\1/p' \
        ${treeSrc}/homelab/local-api/src/routers/custom_app.rs)
      shipped=$(sed -n 's/^version: *"\{0,1\}\([^"]*\)"\{0,1\}$/\1/p' \
        ${treeSrc}/apps/catalog/yolab-common/Chart.yaml)
      if [ -z "$pinned" ] || [ "$pinned" != "$shipped" ]; then
        echo "custom services pin yolab-common '$pinned', but apps/catalog ships '$shipped':" >&2
        echo "set CUSTOM_LIBRARY_VERSION in homelab/local-api/src/routers/custom_app.rs" >&2
        exit 1
      fi
      touch $out
    '';

  only-entry-points-touch-the-machine =
    pkgs.runCommand "only-entry-points-touch-the-machine"
    {
      nativeBuildInputs = [
        pkgs.gnugrep
        pkgs.gawk
        pkgs.findutils
      ];
    }
    ''
      cd ${treeSrc}/homelab/local-api/src
      find . -name '*.rs' \
        ! -path './host.rs' ! -path './ceph_cli.rs' ! -path './exec.rs' \
        ! -path './k8s.rs' ! -path './http.rs' ! -path './main.rs' \
        ! -path './tools.rs' ! -path './runtime/watch.rs' -print0 \
        | xargs -0 awk '/^mod tests/ { nextfile } { print FILENAME ":" FNR ": " $0 }' \
          > "$NIX_BUILD_TOP/scanned"
      grep -E 'RealHost|\bHOST\b|\bkubectl::|ceph_cli::|exec::(output|checked|with_stdin|stream_lines)|k8s::client\(\)|Command::new|reqwest::' \
        "$NIX_BUILD_TOP/scanned" \
        | grep -v 'Backend<H: Host = crate::host::RealHost>' > "$NIX_BUILD_TOP/found" \
        || [ $? -eq 1 ]
      if [ -s "$NIX_BUILD_TOP/found" ]; then
        echo "Only main.rs and tools.rs build the real machine access; everything" >&2
        echo "else takes it from Ctx.tools, AppState, or a 'host: &H' parameter:" >&2
        echo "" >&2
        cat "$NIX_BUILD_TOP/found" >&2
        exit 1
      fi
      touch $out
    '';
  vm-tests-give-swap-room =
    pkgs.runCommand "vm-tests-give-swap-room" {nativeBuildInputs = [pkgs.gnugrep];}
    ''
      problems=""
      for f in ${treeSrc}/nix/tests/*.nix; do
        grep -q 'boot.loader.grub.enable = lib.mkForce true' "$f" || continue

        size=$(grep -oE 'virtualisation\.diskSize = [0-9]+' "$f" | grep -oE '[0-9]+' | head -1)
        if [ -z "$size" ]; then
          problems="$problems\n$(basename "$f"): boots via grub but sets no virtualisation.diskSize"
        elif [ "$size" -lt 4096 ]; then
          problems="$problems\n$(basename "$f"): virtualisation.diskSize=$size is below the 4096 floor"
        fi
      done
      if [ -n "$problems" ]; then
        echo "The VM's root disk defaults to 'auto'-sized to the system" >&2
        echo "closure with zero slack (qemu-vm.nix's additionalSpace =" >&2
        echo "\"0M\", hardcoded, not exposed as an option). services.swapspace" >&2
        echo "(homelab/nixos/common.nix) creates its swapfiles at" >&2
        echo "/var/lib/swapspace, on that same root disk — with no slack" >&2
        echo "there, it can never allocate any swap, so real memory" >&2
        echo "pressure goes straight to the OOM killer instead of being" >&2
        echo "absorbed. This is exactly what OOM-killed coredns mid-test" >&2
        echo "once VM tests got real internet access and their k3s addons" >&2
        echo "started actually pulling and running real images (2026-09-22)." >&2
        echo "Set virtualisation.diskSize to at least 4096 (MiB) on any" >&2
        echo "grub-booted test node:" >&2
        printf "%b\n" "$problems" >&2
        exit 1
      fi
      touch $out
    '';
  tests-name-units-that-exist = let
    svcs = builtins.attrNames nixosSystems.yolab-ci.config.systemd.services;
    timers = builtins.attrNames nixosSystems.yolab-ci.config.systemd.timers;
    known = pkgs.writeText "known-units" (
      pkgs.lib.concatStrings ((map (n: "${n}.service\n") svcs) ++ (map (n: "${n}.timer\n") timers))
    );
  in
    pkgs.runCommand "tests-name-units-that-exist"
    {
      nativeBuildInputs = [
        pkgs.gnugrep
        pkgs.coreutils
      ];
    }
    ''
      LC_ALL=C sort -u ${known} > known

      grep -rhoE '[A-Za-z0-9@_.-]+\.(service|timer)' \
        ${treeSrc}/nix/tests ${treeSrc}/nix/checks \
        | grep -E '^yolab-' \
        | LC_ALL=C sort -u > named

      if ! comm -23 named known > ghosts; then
        echo "could not compare unit names" >&2
        exit 1
      fi

      if [ -s ghosts ]; then
        echo "These unit names appear in nix/tests or nix/checks but no" >&2
        echo "such unit exists in the evaluated NixOS config:" >&2
        echo "" >&2
        sed 's/^/  /' ghosts >&2
        echo "" >&2
        echo "A VM test that waits on, or asserts about, a unit that was" >&2
        echo "renamed or deleted does not fail loudly — systemctl answers for" >&2
        echo "a unit that does not exist, so the assertion quietly stops" >&2
        echo "meaning anything. Every timer assertion in two-node.nix and" >&2
        echo "reboot.nix outlived the timers themselves by nine days this way." >&2
        exit 1
      fi
      touch $out
    '';

  peer-fanout-goes-through-the-fleet =
    pkgs.runCommand "peer-fanout-goes-through-the-fleet"
    {
      nativeBuildInputs = [
        pkgs.gnugrep
        pkgs.diffutils
      ];
    }
    ''
      src=${treeSrc}/homelab/local-api/src

      grep -rl 'peer_ipv6' "$src" \
        | sed "s|^$src/||" \
        | grep -v '^k8s.rs$' \
        | LC_ALL=C sort -u > fanning

      cat > expected <<'EOF'
      routers/reboot.rs
      routers/update.rs
      EOF
      sed 's/^ *//' expected > want

      if ! diff -u want fanning > delta; then
        echo "The set of files that fan an action out to peer machines changed." >&2
        echo "" >&2
        echo "Anything acting on every machine must go through" >&2
        echo "runtime::fleet::rolling, which takes them one at a time and waits" >&2
        echo "for each to come back before touching the next. Both of these did" >&2
        echo "it themselves once: update_all and reboot_all spawned a request" >&2
        echo "per peer and returned, so every machine restarted at the same" >&2
        echo "moment — a guaranteed outage on a cluster of any size, and on two" >&2
        echo "machines an etcd that has to recover from losing both members." >&2
        echo "" >&2
        cat delta >&2
        exit 1
      fi

      for f in $(cat fanning); do
        if ! grep -q 'runtime::fleet' "$src/$f"; then
          echo "$f fans out to peers without using runtime::fleet" >&2
          exit 1
        fi
      done
      touch $out
    '';
  device-plugins-run-on-every-processor =
    pkgs.runCommand "device-plugins-run-on-every-processor" {nativeBuildInputs = [pkgs.gnugrep];}
    ''
      manifest=${../../homelab/nixos/gpu/device-plugins.yaml}
      grep -oE 'image: \S+' "$manifest" | sed 's/image: //' > images
      if grep -v '@sha256:' images; then
        echo "a device plugin image above is not pinned by digest" >&2
        exit 1
      fi
      if grep 'olfillasodikno/' images; then
        echo "upstream generic-cdi-plugin is built for x86 only; run the one CI builds for both" >&2
        exit 1
      fi
      grep -q '^ghcr.io/demycode/generic-cdi-plugin:' images
      touch $out
    '';
}
