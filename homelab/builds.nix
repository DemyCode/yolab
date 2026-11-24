{
  pkgs,
  rust,
}: let
  inherit (pkgs) nodejs;
  version = "0.1.0";

  nodeModules = pkgs.buildNpmPackage {
    pname = "client-ui-node-modules";
    inherit version nodejs;
    src = pkgs.lib.fileset.toSource {
      root = ./client-ui;
      fileset = pkgs.lib.fileset.unions [
        ./client-ui/package.json
        ./client-ui/package-lock.json
      ];
    };
    npmDepsFetcherVersion = 2;
    npmDepsHash = "sha256-56Lvsmn0MPxPIGy3g/sVlAg+RGZmcbGPnHrprQxE+Yw=";
    npmFlags = ["--legacy-peer-deps"];
    dontNpmBuild = true;
    installPhase = ''
      runHook preInstall
      mkdir -p $out
      cp -r node_modules $out/node_modules
      runHook postInstall
    '';
  };

  withNodeModules = pname: run:
    pkgs.stdenv.mkDerivation {
      inherit pname version;
      src = ./client-ui;
      nativeBuildInputs = [nodejs];
      configurePhase = ''
        runHook preConfigure
        export HOME=$TMPDIR
        cp -r ${nodeModules}/node_modules node_modules
        chmod -R u+w node_modules
        runHook postConfigure
      '';
      buildPhase = ''
        runHook preBuild
        ${run}
        runHook postBuild
      '';
      dontInstall = true;
      passthru = {inherit nodeModules;};
    };
in {
  clientUi = withNodeModules "client-ui" ''
    npm run build
    cp -r dist $out
  '';

  clientUiTests = withNodeModules "client-ui-tests" ''
    npm run test
    touch $out
  '';

  clientUiCoverage = withNodeModules "client-ui-coverage" ''
    npm run test:coverage
    cp -r coverage $out
  '';

  clientUiMutants = withNodeModules "client-ui-mutants" ''
    npm run mutants
    cp -r reports $out
  '';

  clientUiUntested = builtins.fromJSON (builtins.readFile ./client-ui/untested.json);

  clientUiLint = withNodeModules "client-ui-lint" ''
    npm run lint
    touch $out
  '';

  localApiEnv = rust.crates.local-api.package;
}
