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
