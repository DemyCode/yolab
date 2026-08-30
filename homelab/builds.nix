# The two artifacts the NixOS module serves: the front-end bundle and the
# local-api binary.
#
# The Rust half is a thin re-export now — the toolchain, the crane setup and
# this crate's build inputs live in ../nix/rust.nix, which the checks and the ISO
# read from too. See the header there for what that consolidation fixed.
# `rust` is required rather than defaulted: a default would let a caller
# silently construct a second toolchain instance, which is the exact thing
# nix/rust.nix exists to prevent.
{
  pkgs,
  rust,
}: let
  # The npm setup, written once. clientUi and clientUiLint are the same
  # dependency tree with a different phase run against it — duplicating this
  # would mean two npmDepsHash values to keep in step, and the second one is
  # exactly the sort of thing that goes stale silently.
  npmArgs = {
    version = "0.1.0";
    src = ./client-ui;
    npmDepsFetcherVersion = 2;
    npmDepsHash = "sha256-cyxr2ViRgiaueoCTNi4yGvECvNOjtIO2y5Yp7zDXfNc=";
    npmFlags = ["--legacy-peer-deps"];
  };
in {
  clientUi = pkgs.buildNpmPackage (npmArgs
    // {
      pname = "client-ui";
      installPhase = ''
        npm run build
        cp -r dist $out
      '';
    });

  # `npm run lint` as a derivation.
  #
  # eslint used to run as a pre-commit hook via sub-pre-commit, and was lost
  # when that file was reduced to calling the checks — which left the front end
  # with a configured linter (homelab/client-ui/eslint.config.js) that nothing
  # executed. tsc is covered by clientUi above, since its installPhase runs
  # `npm run build` = `tsc -b && vite build`; eslint is not, and this is it.
  clientUiLint = pkgs.buildNpmPackage (npmArgs
    // {
      pname = "client-ui-lint";
      dontNpmBuild = true;
      installPhase = ''
        npm run lint
        touch $out
      '';
    });

  localApiEnv = rust.crates.local-api.package;
}
