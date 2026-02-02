#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

TARGET_HOST="${TARGET_HOST:-}"
SSH_KEY="${SSH_KEY:-$HOME/.ssh/id_rsa}"
REPO_URL="${REPO_URL:-https://github.com/yourusername/yolab.git}"
DOMAIN="${DOMAIN:-example.com}"
POSTGRES_DB="${POSTGRES_DB:-frp_services}"
POSTGRES_USER="${POSTGRES_USER:-frp_user}"
POSTGRES_PASSWORD="${POSTGRES_PASSWORD:-}"
IPV6_SUBNET_BASE="${IPV6_SUBNET_BASE:-}"
FRPS_SERVER_IPV6="${FRPS_SERVER_IPV6:-}"

log_info() {
    echo -e "${GREEN}[INFO]${NC} $1"
}

log_warn() {
    echo -e "${YELLOW}[WARN]${NC} $1"
}

log_error() {
    echo -e "${RED}[ERROR]${NC} $1"
}

usage() {
    cat <<EOF
Usage: $0

Required Environment Variables:
  TARGET_HOST         IP address or hostname of target server
  POSTGRES_PASSWORD   PostgreSQL password
  IPV6_SUBNET_BASE    Base IPv6 subnet
  FRPS_SERVER_IPV6    FRP server IPv6 address

Optional Environment Variables:
  SSH_KEY            SSH private key path (default: ~/.ssh/id_rsa)
  REPO_URL           Git repository URL
  DOMAIN             Domain name (default: example.com)
  POSTGRES_DB        Database name (default: frp_services)
  POSTGRES_USER      Database user (default: frp_user)

Example:
  TARGET_HOST=192.168.1.100 \\
  POSTGRES_PASSWORD=secret123 \\
  IPV6_SUBNET_BASE=2001:db8::1:0:0:0 \\
  FRPS_SERVER_IPV6=2001:db8::1 \\
  DOMAIN=yourdomain.com \\
  ./deploy-all-in-one.sh

EOF
    exit 1
}

check_requirements() {
    log_info "Checking requirements..."
    
    for cmd in nix ssh; do
        if ! command -v "$cmd" &> /dev/null; then
            log_error "Required command not found: $cmd"
            exit 1
        fi
    done
    
    if [[ -z "$TARGET_HOST" ]]; then
        log_error "TARGET_HOST is required"
        usage
    fi
    
    if [[ -z "$POSTGRES_PASSWORD" ]]; then
        log_error "POSTGRES_PASSWORD is required"
        usage
    fi
    
    if [[ -z "$IPV6_SUBNET_BASE" ]]; then
        log_error "IPV6_SUBNET_BASE is required"
        usage
    fi
    
    if [[ -z "$FRPS_SERVER_IPV6" ]]; then
        log_error "FRPS_SERVER_IPV6 is required"
        usage
    fi
    
    if [[ ! -f "$SSH_KEY" ]]; then
        log_error "SSH key not found: $SSH_KEY"
        exit 1
    fi
    
    log_info "All requirements satisfied"
}

create_temporary_config() {
    log_info "Creating temporary configuration with secrets..."
    
    local temp_config="$PROJECT_ROOT/deployment/nixos/all-in-one-temp.nix"
    local original_config="$PROJECT_ROOT/deployment/nixos/all-in-one.nix"
    
    sed -e "s|REPLACE_REPO_URL|$REPO_URL|g" \
        -e "s|REPLACE_DOMAIN|$DOMAIN|g" \
        -e "s|REPLACE_POSTGRES_DB|$POSTGRES_DB|g" \
        -e "s|REPLACE_POSTGRES_USER|$POSTGRES_USER|g" \
        -e "s|REPLACE_POSTGRES_PASSWORD|$POSTGRES_PASSWORD|g" \
        -e "s|REPLACE_IPV6_SUBNET_BASE|$IPV6_SUBNET_BASE|g" \
        -e "s|REPLACE_FRPS_SERVER_IPV6|$FRPS_SERVER_IPV6|g" \
        "$original_config" > "$temp_config"
    
    echo "$temp_config"
}

cleanup_temporary_config() {
    local temp_config="$1"
    if [[ -f "$temp_config" ]]; then
        log_info "Cleaning up temporary configuration..."
        rm -f "$temp_config"
    fi
}

deploy() {
    log_info "Starting deployment to $TARGET_HOST..."
    
    local temp_config
    temp_config=$(create_temporary_config)
    
    trap "cleanup_temporary_config '$temp_config'" EXIT
    
    log_info "Building NixOS configuration..."
    cd "$PROJECT_ROOT"
    nix build ".#nixosConfigurations.yolab-server.config.system.build.toplevel" \
        --extra-experimental-features 'nix-command flakes'
    
    log_info "Building disk partitioner..."
    nix build ".#nixosConfigurations.yolab-server.config.system.build.diskoScript" \
        --extra-experimental-features 'nix-command flakes'
    
    log_info "Deploying to $TARGET_HOST with nixos-anywhere..."
    nix run github:nix-community/nixos-anywhere -- \
        --flake ".#yolab-server" \
        --build-on-remote \
        "root@$TARGET_HOST"
    
    log_info "Deployment complete!"
}

main() {
    if [[ "${1:-}" == "-h" ]] || [[ "${1:-}" == "--help" ]]; then
        usage
    fi
    
    check_requirements
    deploy
}

main "$@"
