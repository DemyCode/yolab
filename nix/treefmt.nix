# One formatter for the whole tree.
#
#   nix fmt            # format everything
#   nix run .#format   # identical: the same wrapper, the same config file
#
# The point is that "how do I format this repo" has one answer instead of four
# partial ones. Before this, `nix fmt` did nothing at all (there was no
# `formatter` output), alejandra ran only under pre-commit, prettier ran only
# under pre-commit and only in check mode, and `cargo fmt` was run by nothing —
# 33 Rust files with no formatter attached to them.
#
# The matching gate is `checks.formatting`, built from this same module, so the
# check cannot disagree with what `nix fmt` produces: same binaries, same
# config, same versions, all pinned by flake.lock.
#
# rustToolchain comes from rust.nix, the single definition the checks, the
# packages, the devshell and the ISO all share. `cargo fmt` in the devshell and
# `nix fmt` have to be the same rustfmt, or the two of them rewrite each
# other's output every time the toolchain moves.
{rustToolchain}: {lib, ...}: let
  # Prettier is scoped to the front end by path. Its stock `includes` covers
  # *.json and *.yaml anywhere in the tree, which reaches two places it must
  # not:
  #
  #   apps/catalog/**  helm templates — yaml whose {{ }} its parser rejects,
  #                    plus 89 values.schema.json files nothing else touches.
  #   catalog.yaml     regenerated and committed by the `charts` CI job. Format
  #                    it here and the next publish writes it back unformatted,
  #                    turning main red from a job that did nothing wrong.
  #
  # Two patterns per extension because `a/**/*.ts` does not match a file
  # sitting directly in `a/`.
  ui = ext: [
    "homelab/client-ui/*.${ext}"
    "homelab/client-ui/**/*.${ext}"
  ];
in {
  projectRootFile = "flake.nix";

  # The same formatter the pre-commit hook already runs, so the two agree.
  programs.alejandra.enable = true;

  programs.rustfmt = {
    enable = true;
    package = rustToolchain;
    # treefmt-nix defaults this to 2024; both crates declare edition 2021 and
    # rustfmt parses differently across editions.
    edition = "2021";
  };

  programs.shfmt = {
    enable = true;
    # What apps/wg-register/setup.sh is already written in.
    indent_size = 4;
    # treefmt-nix turns shfmt's -s (simplify) on by default, and that rewrites
    # code, not whitespace — it drops quotes and collapses ${x} to $x. These
    # scripts run under busybox sh inside a shipped image and are driven by
    # setup_test.sh; a formatter has no business making that class of edit.
    simplify = false;
  };

  programs.prettier.enable = true;

  # mkForce, not a plain assignment: `includes` is a bare list option, so
  # setting it here would concatenate with prettier's defaults instead of
  # replacing them — and the defaults are exactly what has to go.
  settings.formatter.prettier.includes = lib.mkForce (
    lib.concatMap ui ["css" "html" "js" "json" "jsx" "md" "mjs" "cjs" "ts" "tsx" "yaml"]
  );

  settings.excludes = [
    # Belt and braces. `checks.formatting` is handed a gitignore-filtered
    # source, so none of these can reach it. But `nix fmt` by hand walks
    # whatever is in front of it, and that is 7.5G of build tree on any machine
    # that has built the workspace once.
    "target/**"
    "node_modules/**"
    "result"
    "result-*"
    # Untracked, holds secrets that exist nowhere else. Already gitignored by
    # homelab/.gitignore; named again here so a change over there cannot
    # quietly start feeding config.toml to a formatter.
    "homelab/ignored/**"
  ];
}
