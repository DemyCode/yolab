{rustToolchain}: {lib, ...}: let
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
    edition = "2021";
  };

  programs.shfmt = {
    enable = true;
    indent_size = 4;
    simplify = false;
  };

  programs.ruff-format.enable = true;
  programs.ruff-check.enable = true;

  programs.prettier.enable = true;

  settings.formatter.prettier.includes = lib.mkForce (
    lib.concatMap ui ["css" "html" "js" "json" "jsx" "md" "mjs" "cjs" "ts" "tsx" "yaml"]
  );

  settings.excludes = [
    "target/**"
    "node_modules/**"
    "result"
    "result-*"
    "homelab/machine/**"
  ];
}
