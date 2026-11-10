{
  pkgs,
  rust,
}: let
  npmArgs = {
    version = "0.1.0";
    src = ./client-ui;
    npmDepsFetcherVersion = 2;
    npmDepsHash = "sha256-1F/w4qhlnJolUoxUxKScwUwWPop6rOmengTJ/OHBjaQ=";
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

  clientUiTests = pkgs.buildNpmPackage (npmArgs
    // {
      pname = "client-ui-tests";
      dontNpmBuild = true;
      installPhase = ''
        npm run test
        touch $out
      '';
    });

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
