# The default `yolab-machine` flake input

Holds placeholder `config.toml` and `hardware-configuration.nix` so
`nixosConfigurations.yolab` is defined on a fresh clone and in CI — without them
`flake.nix` defines no `yolab` at all, and nothing can build or cache it.

They are **not** a machine. A real machine keeps its own files outside the repo,
in `/var/lib/yolab/machine/`:

- `config.toml` — written by the installer
- `hardware-configuration.nix` — written by the installer (NixOS only)

and builds with:

```bash
nixos-rebuild switch --flake /etc/nixos#yolab \
  --override-input yolab-machine path:/var/lib/yolab/machine \
  --no-write-lock-file
```

The installer does the same thing implicitly: it writes the machine's files into
its clone of this repo and builds `path:<clone>#yolab`, so the placeholders are
overwritten before anything is built.

Never put a real machine's secrets here — this directory is part of the repo.
