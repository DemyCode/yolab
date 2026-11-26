{
  lib,
  x86,
  vm,
  ...
}:
lib.optionalAttrs x86 (lib.mapAttrs (_: test: import test vm) {
  boot-test = ../tests/boot.nix;
  two-node-test = ../tests/two-node.nix;
  disk-loss-test = ../tests/disk-loss.nix;
  reboot-test = ../tests/reboot.nix;
  rolling-reboot-test = ../tests/rolling-reboot.nix;
  two-node-disk-loss-test = ../tests/two-node-disk-loss.nix;
})
