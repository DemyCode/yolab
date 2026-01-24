# Building for NixOS

## Initial Setup

The NixOS module needs the npm dependencies hash. To compute it:

1. First build attempt will fail with hash mismatch
2. Copy the correct hash from error message
3. Update `modules/client-ui.nix` with correct hash

Example error:
```
error: hash mismatch in fixed-output derivation
  got:    sha256-XXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXX=
```

Copy the hash and update in `modules/client-ui.nix`:
```nix
npmDepsHash = "sha256-XXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXX=";
```

## Alternative: Use lib.fakeHash

For development, use:
```nix
npmDepsHash = pkgs.lib.fakeHash;
```

Then run:
```bash
nix build .#yolab
```

The build will fail and show the correct hash.
