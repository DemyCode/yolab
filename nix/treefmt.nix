# One formatter for the whole tree.
#
#   nix fmt            # format everything
#   nix run .#format   # identical wrapper, same config
#
# `checks.formatting` is built from this same module, so the gate and the fixer
# cannot disagree.
#
# rustToolchain comes from nix/rust.nix: `cargo fmt` in the devshell and
# `nix fmt` must be the same rustfmt, or they rewrite each other on every
# toolchain bump.
{rustToolchain}: {lib, ...}: let
  # Two patterns per extension because `a/**/*.ts` does not match a file
  # sitting directly in `a/`.
  ui = ext: [
    "homelab/client-ui/*.${ext}"
    "homelab/client-ui/**/*.${ext}"
  ];
in {
  projectRootFile = "flake.nix";

  programs.alejandra.enable = true;

  programs.rustfmt = {
    enable = true;
    package = rustToolchain;
    # treefmt-nix defaults to 2024; both crates are 2021 and rustfmt parses
    # differently across editions.
    edition = "2021";
  };

  programs.shfmt = {
    enable = true;
    indent_size = 4;
    # -s rewrites code rather than whitespace — it drops quotes and collapses
    # ${x} to $x. Not a formatter's job.
    simplify = false;
  };

  programs.ruff-format.enable = true;
  programs.ruff-check.enable = true;

  programs.prettier.enable = true;

  # Scoped to the front end. Prettier's stock includes cover *.json and *.yaml
  # everywhere, which would reach apps/catalog — helm templates whose {{ }} its
  # parser rejects, plus 89 hand-maintained values.schema.json — and
  # catalog.yaml, which the charts CI job regenerates and would then turn red.
  #
  # mkForce because `includes` is a plain list option: assigning would append
  # to those defaults rather than replace them.
  settings.formatter.prettier.includes = lib.mkForce (
    lib.concatMap ui ["css" "html" "js" "json" "jsx" "md" "mjs" "cjs" "ts" "tsx" "yaml"]
  );

  settings.excludes = [
    # `checks.formatting` gets a filtered source, but `nix fmt` by hand walks
    # whatever is in front of it.
    "target/**"
    "node_modules/**"
    "result"
    "result-*"
    # Secrets. Gitignored already; named here so a change there cannot quietly
    # start feeding config.toml to a formatter.
    "homelab/ignored/**"
  ];
}
