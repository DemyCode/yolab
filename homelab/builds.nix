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
}: {
  clientUi = pkgs.buildNpmPackage {
    pname = "client-ui";
    version = "0.1.0";
    src = ./client-ui;
    npmDepsFetcherVersion = 2;
    npmDepsHash = "sha256-cyxr2ViRgiaueoCTNi4yGvECvNOjtIO2y5Yp7zDXfNc=";
    npmFlags = ["--legacy-peer-deps"];
    installPhase = ''
      npm run build
      cp -r dist $out
    '';
  };

  localApiEnv = rust.crates.local-api.package;
}
