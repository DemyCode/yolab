#!/usr/bin/env bash
set -e

# YoLab Health Check Script
# Check status of deployed services

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
TERRAFORM_DIR="$PROJECT_ROOT/deployment/terraform"

# Colors
GREEN='\033[0;32m'
RED='\033[0;31m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

info() {
    echo -e "${BLUE}[INFO]${NC} $1"
}

success() {
    echo -e "${GREEN}[✓]${NC} $1"
}

error() {
    echo -e "${RED}[✗]${NC} $1"
}

warning() {
    echo -e "${YELLOW}[!]${NC} $1"
}

check_service() {
    local name=$1
    local command=$2
    
    if eval "$command" &> /dev/null; then
        success "$name"
    else
        error "$name"
        return 1
    fi
}

main() {
    echo ""
    echo "╔════════════════════════════════════════════════════╗"
    echo "║        YoLab Health Check v1.0                     ║"
    echo "╚════════════════════════════════════════════════════╝"
    echo ""
    
    if [ ! -d "$TERRAFORM_DIR/.terraform" ]; then
        error "Terraform not initialized. Run deploy.sh first."
        exit 1
    fi
    
    cd "$TERRAFORM_DIR"
    
    info "Fetching server information..."
    FRPS_IP=$(terraform output -raw frps_server_ipv4 2>/dev/null || echo "")
    SERVICES_IP=$(terraform output -raw services_server_ipv4 2>/dev/null || echo "")
    DOMAIN=$(terraform output -json dns_configuration 2>/dev/null | jq -r '.domain' || echo "")
    
    if [ -z "$FRPS_IP" ] || [ -z "$SERVICES_IP" ]; then
        error "Could not get server IPs. Is infrastructure deployed?"
        exit 1
    fi
    
    echo ""
    echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
    echo "  SERVER CONNECTIVITY"
    echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
    check_service "FRPS Server SSH ($FRPS_IP)" "ssh -o ConnectTimeout=5 -o StrictHostKeyChecking=no root@$FRPS_IP 'exit'"
    check_service "Services Server SSH ($SERVICES_IP)" "ssh -o ConnectTimeout=5 -o StrictHostKeyChecking=no root@$SERVICES_IP 'exit'"
    
    echo ""
    echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
    echo "  FRPS SERVER STATUS"
    echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
    check_service "FRPS Service Running" "ssh -o ConnectTimeout=5 -o StrictHostKeyChecking=no root@$FRPS_IP 'systemctl is-active frps'"
    check_service "FRPS Port 7000 Open" "nc -z -w5 $FRPS_IP 7000"
    
    echo ""
    echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
    echo "  SERVICES SERVER STATUS"
    echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
    check_service "Docker Running" "ssh -o ConnectTimeout=5 -o StrictHostKeyChecking=no root@$SERVICES_IP 'docker ps > /dev/null'"
    check_service "Backend Container" "ssh -o ConnectTimeout=5 -o StrictHostKeyChecking=no root@$SERVICES_IP 'docker ps | grep frp-backend'"
    check_service "PostgreSQL Container" "ssh -o ConnectTimeout=5 -o StrictHostKeyChecking=no root@$SERVICES_IP 'docker ps | grep frp-postgres'"
    check_service "DNS Server Container" "ssh -o ConnectTimeout=5 -o StrictHostKeyChecking=no root@$SERVICES_IP 'docker ps | grep frp-dns-server'"
    
    echo ""
    echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
    echo "  API HEALTH CHECKS"
    echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
    check_service "Backend API Health" "curl -f -s http://$SERVICES_IP:5000/health | grep -q healthy"
    check_service "Backend Port 5000 Open" "nc -z -w5 $SERVICES_IP 5000"
    check_service "DNS Port 53 Open" "nc -z -w5 -u $SERVICES_IP 53"
    
    if [ -n "$DOMAIN" ]; then
        echo ""
        echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
        echo "  DNS CONFIGURATION"
        echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
        check_service "Domain resolves ($DOMAIN)" "dig +short $DOMAIN | grep -q $FRPS_IP"
        check_service "API subdomain resolves (api.$DOMAIN)" "dig +short api.$DOMAIN | grep -q $SERVICES_IP"
    fi
    
    echo ""
    echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
    echo "  QUICK COMMANDS"
    echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
    echo ""
    echo "SSH to servers:"
    echo "  FRPS:     ssh root@$FRPS_IP"
    echo "  Services: ssh root@$SERVICES_IP"
    echo ""
    echo "View logs:"
    echo "  FRPS:    ssh root@$FRPS_IP 'journalctl -u frps -f'"
    echo "  Backend: ssh root@$SERVICES_IP 'docker logs -f frp-backend'"
    echo ""
    echo "Test API:"
    echo "  curl http://$SERVICES_IP:5000/health"
    echo ""
    echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
    echo ""
}

main
