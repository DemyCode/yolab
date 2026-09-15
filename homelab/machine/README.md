# The default `yolab-machine` flake input

Deliberately empty. `flake.nix` defines the `yolab` machine only when the
`yolab-machine` input holds a `config.toml`, so pointing at this directory — what
CI and a fresh clone do — evaluates without any machine's secrets.

A real machine keeps its files outside the repo, in `/var/lib/yolab/machine/`:

- `config.toml` — written by the installer
- `hardware-configuration.nix` — written by the installer (NixOS only)

and builds with:

```bash
nixos-rebuild switch --flake /etc/nixos#yolab \
  --override-input yolab-machine path:/var/lib/yolab/machine \
  --no-write-lock-file
```

Never put a machine's files here: this directory is part of the repo.
