#!/usr/bin/env bash
set -e

# YoLab Destruction Script
# This script destroys all deployed infrastructure

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
TERRAFORM_DIR="$PROJECT_ROOT/deployment/terraform"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

error() {
    echo -e "${RED}[ERROR]${NC} $1"
    exit 1
}

warning() {
    echo -e "${YELLOW}[WARNING]${NC} $1"
}

info() {
    echo -e "${BLUE}[INFO]${NC} $1"
}

success() {
    echo -e "${GREEN}[SUCCESS]${NC} $1"
}

# Main
main() {
    echo ""
    echo "╔════════════════════════════════════════════════════╗"
    echo "║        YoLab Destruction Script v1.0               ║"
    echo "║  ⚠️  DANGER: This will DELETE all resources! ⚠️   ║"
    echo "╚════════════════════════════════════════════════════╝"
    echo ""
    
    if [ ! -d "$TERRAFORM_DIR/.terraform" ]; then
        error "Terraform not initialized. Nothing to destroy."
    fi
    
    cd "$TERRAFORM_DIR"
    
    # Show what will be destroyed
    warning "The following resources will be DESTROYED:"
    echo ""
    terraform state list
    echo ""
    
    warning "This action CANNOT be undone!"
    warning "All data will be PERMANENTLY lost!"
    echo ""
    
    read -p "Type 'destroy' to confirm: " confirm
    
    if [ "$confirm" != "destroy" ]; then
        info "Destruction cancelled"
        exit 0
    fi
    
    info "Destroying infrastructure..."
    terraform destroy
    
    success "All resources have been destroyed"
    echo ""
}

main
