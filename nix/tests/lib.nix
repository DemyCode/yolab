{pkgs}: {
  # Real internet access for exactly this VM test's build — nothing else in
  # the flake. `nix build` sandboxes every derivation (no network) unless it
  # is a fixed-output derivation; there is no per-derivation "give this one
  # network" option in the nixosTest framework itself (nixpkgs' own run.nix
  # says as much: "TODO: can the interactive driver be configured to access
  # the network?"). `__noChroot` is Nix's actual escape hatch for that, and
  # `overrideTestDerivation` (the documented way to reach into a nixosTest's
  # underlying mkDerivation, see nixos/lib/testing/run.nix) is what lets it
  # reach this one test's derivation without a flake-wide nixConfig.sandbox
  # setting that would also de-sandbox every Rust build, chart check and ISO
  # build in the flake. Requires the building user to be `trusted-users` in
  # nix.conf (this machine has `nixos`; cachix/install-nix-action's default
  # CI setup makes the runner user trusted too) — an untrusted user silently
  # gets the normal sandboxed build instead, not an error.
  withNetwork = test: test.overrideTestDerivation (_: {__noChroot = true;});

  # A tiny OCI image built entirely from the Nix store — no registry pull, so
  # it works inside a VM test's network-sandboxed VM. `k3s ctr -n k8s.io
  # images import` loads it into containerd's local store before any pod
  # references it; pair with `imagePullPolicy: Never` so k3s never tries the
  # network anyway. This is what makes it possible for a VM test to assert a
  # pod actually reaches Ready, not just that the Deployment object exists —
  # busybox from docker.io can never do that here: see the comment on
  # rook-ceph-namespace in two-node.nix for why nothing in this sandbox can
  # reach the internet, ever, on any runner.
  demoImageName = "yolab-test-demo";
  demoImageTag = "latest";
  demoImage = pkgs.dockerTools.buildImage {
    name = "yolab-test-demo";
    tag = "latest";
    config.Cmd = ["${pkgs.coreutils}/bin/sleep" "infinity"];
  };

  machine = {
    configPath,
    systemDisk ? "/dev/vdb",
  }: {
    systemd.tmpfiles.rules = [
      "C /var/lib/yolab/machine/config.toml 0600 root root - ${configPath}"
    ];

    systemd.services.yolab-test-system-lv = {
      description = "The system LV disko would have created (VM test)";
      before = ["yolab-local-api.service"];
      wantedBy = ["multi-user.target"];
      after = ["local-fs.target"];
      path = [pkgs.lvm2 pkgs.util-linux];
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
      };
      script = ''
        if [ -e /dev/mapper/pool-ceph ]; then exit 0; fi
        wipefs -a ${systemDisk} || true
        pvcreate -ff -y ${systemDisk}
        vgcreate pool ${systemDisk}
        lvcreate -y -l 100%FREE -n ceph pool
        vgchange -ay pool
        udevadm settle
      '';
    };
  };

  preamble = ''
    import json

    YOLAB_UNITS = [
        "yolab-reset-wipe",
        "yolab-ceph-bootstrap",
        "k3s",
        "yolab-local-api",
    ]

    YOLAB_SNAPSHOT = "/run/yolab/controllers.json"


    def yolab_snapshot(machine):
        """yolabd's own view of every resource it supervises."""
        return json.loads(machine.succeed(f"cat {YOLAB_SNAPSHOT}"))


    def yolab_controllers(machine):
        return {c["name"]: c for c in yolab_snapshot(machine)["controllers"]}


    def assert_yolabd_is_ticking(machines, names, settle=90):
        """The modern form of "every self-healing timer is armed".

        There are no systemd timers any more — commit 998caf8 dropped them when
        the controller runtime landed, and the timer assertions these replace
        outlived them, asserting about units that no longer existed.
        """
        machines = machines if isinstance(machines, list) else [machines]
        before = {m.name: yolab_controllers(m) for m in machines}
        for m in machines:
            for n in names:
                assert n in before[m.name], (
                    f"{n} is not registered on {m.name} — yolabd supervises "
                    f"{sorted(before[m.name])}"
                )

        for m in machines:
            m.sleep(settle)

        after = {m.name: yolab_controllers(m) for m in machines}
        for m in machines:
            moved = [
                n
                for n in after[m.name]
                if after[m.name][n]["runs"] > before[m.name].get(n, {}).get("runs", 0)
            ]
            assert moved, (
                f"not one of yolabd's {len(after[m.name])} resources ran on "
                f"{m.name} in {settle}s — the supervisor has stopped ticking"
            )
            for n in names:
                b, a = before[m.name][n], after[m.name][n]
                interval = max(int(b["interval_secs"]), 1)
                delta = a["runs"] - b["runs"]
                cap = settle // interval + 3
                assert delta <= cap, (
                    f"{n} on {m.name} ran {delta} times in {settle}s with a "
                    f"{interval}s interval (cap {cap}) — it is hot-looping, "
                    "which is what a retry that measures from the start of the "
                    "run instead of its end does"
                )


    def yolab_diagnose(machine, what):
        """Everything needed to tell why the machine did not get where it was going."""
        print(f"\n{'=' * 72}\n== {machine.name}: {what}\n{'=' * 72}")

        for label, cmd in (
            ("failed units", "systemctl list-units --failed --no-pager --plain"),
            ("jobs still running", "systemctl list-jobs --no-pager"),
            ("ceph", "timeout 15 ceph -s --connect-timeout 10 2>&1 | head -30"),
            ("block devices", "lsblk -o NAME,SIZE,TYPE,MOUNTPOINT 2>&1"),
            ("memory", "free -m"),
            ("yolabd", f"cat {YOLAB_SNAPSHOT} 2>&1 | head -60"),
        ):
            status, out = machine.execute(cmd)
            print(f"\n-- {machine.name} {label} (exit {status}) --\n{out}")

        for unit in YOLAB_UNITS:
            status, state = machine.execute(
                f"systemctl show -p ActiveState -p SubState -p Result --value {unit}.service"
            )
            state = " ".join(state.split())
            print(f"\n-- {machine.name} {unit}: {state} --")
            _, log = machine.execute(f"journalctl -u {unit}.service --no-pager -n 40 2>&1")
            print(log)

    class step:
        """Wrap an assertion so a failure explains itself.

        with step(node1, "k3s comes up"):
            node1.wait_for_unit("k3s.service", timeout=600)
        """

        def __init__(self, machines, what):
            self.machines = machines if isinstance(machines, list) else [machines]
            self.what = what

        def __enter__(self):
            print(f"\n>>> {self.what}")
            return self

        def __exit__(self, exc_type, exc, tb):
            if exc_type is not None:
                for m in self.machines:
                    yolab_diagnose(m, f"FAILED: {self.what}")
            return False
  '';
}
