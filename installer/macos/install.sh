#!/usr/bin/env bash
set -euo pipefail

YOLAB_REPO="${YOLAB_REPO:-https://github.com/DemyCode/yolab.git}"
YOLAB_DIR="/opt/yolab"
MACHINE_DIR="/var/lib/yolab/machine"
MACHINE_FLAGS=(--override-input yolab-machine "path:$MACHINE_DIR" --no-write-lock-file)
NIX_INSTALLER_URL="https://install.determinate.systems/nix"

ARCH=$(uname -m)
if [ "$ARCH" = "arm64" ]; then
    FLAKE_TARGET="yolab-mac"
else
    FLAKE_TARGET="yolab-mac-x86"
fi

step() {
    echo
    echo ">>> $*"
}
ok() { echo "    ✓ $*"; }
warn() { echo "    ⚠  $*"; }

require_command() {
    if ! command -v "$1" &>/dev/null; then
        echo "ERROR: '$1' not found. $2"
        exit 1
    fi
}

step "Checking Xcode Command Line Tools"
if ! xcode-select -p &>/dev/null; then
    echo "    Installing Xcode Command Line Tools..."
    xcode-select --install
    echo "    Please complete the installation dialog, then re-run this script."
    exit 0
fi
ok "Xcode CLT present"

step "Checking Nix installation"
if command -v nix &>/dev/null; then
    ok "Nix already installed: $(nix --version)"
else
    echo "    Installing Nix via Determinate Systems installer..."
    curl --proto '=https' --tlsv1.2 -sSf -L "$NIX_INSTALLER_URL" | sh -s -- install
    # shellcheck disable=SC1091
    . /nix/var/nix/profiles/default/etc/profile.d/nix-daemon.sh
    ok "Nix installed"
fi

step "Setting up YoLab repository at $YOLAB_DIR"
if [ -d "$YOLAB_DIR/.git" ]; then
    warn "Repository already exists — pulling latest changes."
    sudo git -C "$YOLAB_DIR" pull
else
    sudo mkdir -p "$YOLAB_DIR"
    sudo git clone "$YOLAB_REPO" "$YOLAB_DIR"
fi
sudo chmod -R a+rX "$YOLAB_DIR"
ok "Repository ready at $YOLAB_DIR"

step "Collecting homelab configuration"
echo "    Running setup wizard..."

sudo python3 "$YOLAB_DIR/installer/macos/setup.py" "$YOLAB_DIR" "$FLAKE_TARGET"

step "Bootstrapping nix-darwin"

if command -v darwin-rebuild &>/dev/null; then
    ok "nix-darwin already installed"
    darwin-rebuild switch --flake "$YOLAB_DIR#$FLAKE_TARGET" "${MACHINE_FLAGS[@]}"
else
    echo "    Installing nix-darwin for the first time..."
    nix run nix-darwin -- switch --flake "$YOLAB_DIR#$FLAKE_TARGET" "${MACHINE_FLAGS[@]}"
fi
ok "nix-darwin applied: $FLAKE_TARGET"

step "Starting colima (Docker)"
if colima status 2>/dev/null | grep -q "running"; then
    ok "colima already running"
else
    colima start --runtime docker
    ok "colima started"
fi

echo
echo "========================================"
echo " YoLab installation complete!"
echo "========================================"
echo
echo "  UI: http://localhost"
echo "  To update: click 'Update homelab' in the UI"
echo "         or: darwin-rebuild switch --flake $YOLAB_DIR#$FLAKE_TARGET ${MACHINE_FLAGS[*]}"
echo
