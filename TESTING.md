# How yolab is tested

Five tiers. Each one is cheaper and more specific than the one below it, so a
property belongs in the highest tier that can actually hold it. Putting a
property lower than it needs to be is the expensive mistake: a VM test that
could have been a `FakeHost` test costs eight minutes and a runner every push,
forever.

| tier | where | needs | runs |
|---|---|---|---|
| 1. Nix invariants | `nix/checks.nix` | nothing | every push, `lint`/`nixos` bucket |
| 2. Rust unit | `#[cfg(test)] mod tests` beside the code | nothing | every push, `rust` bucket |
| 3. Rust seam | same, driven by `host::fake::FakeHost` | nothing | every push, `rust` bucket |
| 4. Rust surface | `surface.rs` + `testkit.rs`, the real axum router | nothing | every push, `rust` bucket |
| 5. NixOS VM | `nix/tests/*.nix` | `/dev/kvm` | every push, one runner per test |

## 1. Nix invariants — the tier nobody else has

We build the OS declaratively, so a whole class of property can be proven by
reading the *evaluated* config, with no machine and no boot: unit ordering,
timer semantics, which binaries a unit's `PATH` provides, whether a hand-written
list still matches the thing it describes. These cost milliseconds.

Reach for this tier first. `self-healing-timers-can-re-arm` catches, statically,
a bug that took a node offline for 32 hours — and it would take a 32-hour VM
test to catch the same thing at tier 5.

## 2–3. Rust unit and seam tests

`host.rs` defines the `Host` trait: every kubectl, ceph, ceph-volume, systemctl
and lsblk call goes through it. `host::fake::FakeHost` answers from a script and
records what was asked, so a reconcile loop's real failure paths — a timeout, a
`NotFound`, a command that succeeds but returns nonsense — can be exercised
without a cluster.

`heal/` is the reference: generic over `H: Host, N: Network`, with both faked.
Code that names `RealHost` or `crate::kubectl::` directly has stepped around the
seam and cannot be tested at this tier at all. `host-seam-ratchet` in
`nix/checks.nix` holds a per-file budget for that, and the budget may only
shrink.

## 4. Surface tests

`testkit::TestApi` stands up the **real** router from `router.rs` — the real auth
layer, the real cache layer, in the real order — and drives it with
`tower::ServiceExt::oneshot`. Never assemble a `Router` in a test: that proves a
handler works, not that it is reachable, not that it is behind auth, and not that
it is mounted where the UI asks for it. All three have been wrong here.

`surface::ROUTE_TABLE` lists every registered path, and
`route-table-is-complete` diffs it against `router.rs` in both directions, so
cross-cutting sweeps like `every_route_refuses_an_unauthenticated_stranger`
cover a new route the moment it is added.

## 5. NixOS VM tests

Real machines, booted, with real disks. This is where storage, boot ordering,
quorum and recovery live, because nothing below tier 5 can observe them. They
need `/dev/kvm`; CI gives each one its own runner, and the matrix is
`builtins.attrNames` of the flake's `nixosTests`, so adding a test is enough to
get it run.

Borrowed from umbrelOS, which runs ~95 of these per push and finds its RAID bugs
in CI rather than on customers' machines.

## The rule for incidents

**Every outage becomes a test in the highest tier that can hold it, in the same
change that fixes it.**

This repo has a habit of writing the incident into a comment instead. Those
comments are excellent and they are not enforcement: a comment explaining that a
`RemainAfterExit` oneshot kills its own timer did not stop three more units from
doing it. Write the test; keep the *why* next to the test, where it explains
what the assertion is for, rather than next to the code, where it explains what
the reader must remember not to do.

A comment that asserts a property is a test that has not been written yet.

## Running things

```sh
nix run .#ci                        # every check, exactly as CI does
nix build .#checks.x86_64-linux.<name>   # one check
nix build .#nixosTests.two-node-test     # one VM test (needs /dev/kvm)
nix run .#coverage                  # local-api + installer coverage reports
```
