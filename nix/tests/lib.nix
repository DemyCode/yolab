# Shared Python preamble for every yolab VM test.
#
# WHY THIS EXISTS. A NixOS VM test that fails prints the assertion that failed
# and nothing else. For a test whose subject is "did this machine finish coming
# up", that is the least useful half of the story: `systemctl is-active
# k3s.service` returning "activating" tells you k3s is not up, and says nothing
# about the four units ahead of it in the boot line, any one of which is the
# actual reason. Someone then has to reproduce a ten-minute VM boot locally to
# learn something the failing run already knew.
#
# So every test wraps its assertions in `step(...)`, and a failure dumps the
# boot line's state before re-raising. The log becomes the diagnosis.
{
  preamble = ''
    # The boot line to k3s, in order, plus the daemon that reports on it. A
    # failure anywhere here explains every failure after it — see the
    # containerd-store-after-order check in nix/checks.nix for the ordering.
    YOLAB_UNITS = [
        "yolab-reset-wipe",
        "yolab-ceph-bootstrap",
        "yolab-ceph-system-osd",
        "yolab-ceph-osd-activate",
        "yolab-images-rbd",
        "yolab-containerd-store",
        "k3s",
        "yolab-local-api",
    ]


    def yolab_diagnose(machine, what):
        """Everything needed to tell why the machine did not get where it was going."""
        print(f"\n{'=' * 72}\n== {machine.name}: {what}\n{'=' * 72}")

        for label, cmd in (
            ("failed units", "systemctl list-units --failed --no-pager --plain"),
            ("jobs still running", "systemctl list-jobs --no-pager"),
            ("ceph", "timeout 15 ceph -s --connect-timeout 10 2>&1 | head -30"),
            ("block devices", "lsblk -o NAME,SIZE,TYPE,MOUNTPOINT 2>&1"),
            ("memory", "free -m"),
        ):
            status, out = machine.execute(cmd)
            print(f"\n-- {machine.name} {label} (exit {status}) --\n{out}")

        for unit in YOLAB_UNITS:
            status, state = machine.execute(
                f"systemctl show -p ActiveState -p SubState -p Result --value {unit}.service"
            )
            state = " ".join(state.split())
            print(f"\n-- {machine.name} {unit}: {state} --")
            # Only the tail: a unit that is fine needs one line, and a unit that
            # is not puts its reason at the end.
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
