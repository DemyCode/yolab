# The two artifacts the NixOS module serves. The Rust half comes from
# ../nix/rust.nix; `rust` is required rather than defaulted so no caller can
# silently construct a second toolchain instance.
{
  pkgs,
  rust,
}: let
  # Shared so there is one npmDepsHash rather than two to keep in step.
  npmArgs = {
    version = "0.1.0";
    src = ./client-ui;
    npmDepsFetcherVersion = 2;
    npmDepsHash = "sha256-C4mIJBwvGu3B4xbxyP8cZoSfMHX10nKFEBri3xof+lE=";
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

  # The vitest suite. Its own derivation rather than a step inside `clientUi`:
  # a test run and a production bundle fail for different reasons and should
  # say so separately, and this one has no build output to install.
  #
  # The UI had no test runner at all until 2026-09-22 — 10k lines of TypeScript
  # whose only gate was `tsc`, which proves types line up and nothing about
  # behaviour. The first test written against it found formatBytes rendering
  # "1.0 GB" directly under a comment promising it never would.
  clientUiTests = pkgs.buildNpmPackage (npmArgs
    // {
      pname = "client-ui-tests";
      dontNpmBuild = true;
      installPhase = ''
        npm run test
        touch $out
      '';
    });

  # tsc is covered by clientUi, whose build runs `tsc -b && vite build`.
  # eslint is not, and this is it.
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
