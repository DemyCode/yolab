#!/usr/bin/env bash
set -e

# YoLab Deployment Script
# This script guides you through deploying YoLab to Hetzner Cloud

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
TERRAFORM_DIR="$PROJECT_ROOT/deployment/terraform"

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

# Helper functions
info() {
    echo -e "${BLUE}[INFO]${NC} $1"
}

success() {
    echo -e "${GREEN}[SUCCESS]${NC} $1"
}

warning() {
    echo -e "${YELLOW}[WARNING]${NC} $1"
}

error() {
    echo -e "${RED}[ERROR]${NC} $1"
    exit 1
}

prompt() {
    echo -e "${YELLOW}[PROMPT]${NC} $1"
    read -p "> " response
    echo "$response"
}

# Check prerequisites
check_prerequisites() {
    info "Checking prerequisites..."
    
    if ! command -v terraform &> /dev/null; then
        error "Terraform is not installed. Please install it first."
    fi
    
    if ! command -v git &> /dev/null; then
        error "Git is not installed. Please install it first."
    fi
    
    if ! command -v ssh &> /dev/null; then
        error "SSH is not installed. Please install it first."
    fi
    
    success "All prerequisites are met!"
}

# Check if terraform.tfvars exists
check_tfvars() {
    info "Checking Terraform configuration..."
    
    if [ ! -f "$TERRAFORM_DIR/terraform.tfvars" ]; then
        warning "terraform.tfvars not found!"
        echo ""
        echo "Please create $TERRAFORM_DIR/terraform.tfvars from the example:"
        echo "  cd $TERRAFORM_DIR"
        echo "  cp terraform.tfvars.example terraform.tfvars"
        echo "  vim terraform.tfvars"
        echo ""
        error "Configuration file missing"
    fi
    
    success "Terraform configuration found!"
}

# Initialize Terraform
init_terraform() {
    info "Initializing Terraform..."
    
    cd "$TERRAFORM_DIR"
    
    if [ ! -d ".terraform" ]; then
        terraform init
        success "Terraform initialized!"
    else
        info "Terraform already initialized"
    fi
}

# Show deployment plan
show_plan() {
    info "Generating deployment plan..."
    
    cd "$TERRAFORM_DIR"
    terraform plan -out=tfplan
    
    echo ""
    warning "Please review the plan above carefully!"
    echo ""
}

# Deploy infrastructure
deploy() {
    cd "$TERRAFORM_DIR"
    
    response=$(prompt "Do you want to proceed with deployment? (yes/no)")
    
    if [ "$response" != "yes" ]; then
        info "Deployment cancelled"
        exit 0
    fi
    
    info "Starting deployment..."
    terraform apply tfplan
    
    success "Deployment complete!"
    echo ""
    
    # Show outputs
    info "Deployment Information:"
    echo ""
    terraform output
}

# Post-deployment checks
post_deployment_checks() {
    info "Running post-deployment checks..."
    
    cd "$TERRAFORM_DIR"
    
    FRPS_IP=$(terraform output -raw frps_server_ipv4 2>/dev/null || echo "")
    SERVICES_IP=$(terraform output -raw services_server_ipv4 2>/dev/null || echo "")
    
    if [ -z "$FRPS_IP" ] || [ -z "$SERVICES_IP" ]; then
        warning "Could not get server IPs from Terraform output"
        return
    fi
    
    echo ""
    info "Waiting for servers to be ready (30 seconds)..."
    sleep 30
    
    # Check FRPS server
    info "Checking FRPS server ($FRPS_IP)..."
    if ssh -o ConnectTimeout=10 -o StrictHostKeyChecking=no root@"$FRPS_IP" "systemctl is-active frps" &> /dev/null; then
        success "FRPS server is running!"
    else
        warning "FRPS server might not be ready yet. Check with: ssh root@$FRPS_IP"
    fi
    
    # Check services server
    info "Checking services server ($SERVICES_IP)..."
    if ssh -o ConnectTimeout=10 -o StrictHostKeyChecking=no root@"$SERVICES_IP" "docker ps" &> /dev/null; then
        success "Services server is running!"
    else
        warning "Services might not be ready yet. Check with: ssh root@$SERVICES_IP"
    fi
    
    # Try to check backend health
    info "Checking backend API..."
    if curl -f -s "http://$SERVICES_IP:5000/health" &> /dev/null; then
        success "Backend API is healthy!"
    else
        warning "Backend API not responding yet. It might still be starting up."
    fi
}

# Show DNS configuration
show_dns_config() {
    cd "$TERRAFORM_DIR"
    
    DOMAIN=$(terraform output -json dns_configuration 2>/dev/null | jq -r '.domain' || echo "")
    FRPS_IP=$(terraform output -raw frps_server_ipv4 2>/dev/null || echo "")
    FRPS_IP6=$(terraform output -raw frps_server_ipv6 2>/dev/null || echo "")
    SERVICES_IP=$(terraform output -raw services_server_ipv4 2>/dev/null || echo "")
    SERVICES_IP6=$(terraform output -raw services_server_ipv6 2>/dev/null || echo "")
    
    if [ -z "$DOMAIN" ]; then
        warning "Could not get domain from Terraform output"
        return
    fi
    
    echo ""
    echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
    echo "  DNS CONFIGURATION REQUIRED"
    echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
    echo ""
    echo "Add these DNS records to your domain provider:"
    echo ""
    echo "A Records (IPv4):"
    echo "  @                    A      $FRPS_IP"
    echo "  *.$DOMAIN            A      $FRPS_IP"
    echo "  api                  A      $SERVICES_IP"
    echo ""
    echo "AAAA Records (IPv6):"
    echo "  @                    AAAA   $FRPS_IP6"
    echo "  *.$DOMAIN            AAAA   $FRPS_IP6"
    echo "  api                  AAAA   $SERVICES_IP6"
    echo ""
    echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
}

# Show next steps
show_next_steps() {
    cd "$TERRAFORM_DIR"
    
    FRPS_IP=$(terraform output -raw frps_server_ipv4 2>/dev/null || echo "N/A")
    SERVICES_IP=$(terraform output -raw services_server_ipv4 2>/dev/null || echo "N/A")
    
    echo ""
    echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
    echo "  NEXT STEPS"
    echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
    echo ""
    echo "1. Configure DNS (see above)"
    echo ""
    echo "2. Verify servers are running:"
    echo "   SSH to FRPS:     ssh root@$FRPS_IP"
    echo "   SSH to Services: ssh root@$SERVICES_IP"
    echo ""
    echo "3. Check service status:"
    echo "   FRPS:    ssh root@$FRPS_IP 'systemctl status frps'"
    echo "   Docker:  ssh root@$SERVICES_IP 'docker ps'"
    echo "   Backend: curl http://$SERVICES_IP:5000/health"
    echo ""
    echo "4. View logs:"
    echo "   FRPS:    ssh root@$FRPS_IP 'journalctl -u frps -f'"
    echo "   Backend: ssh root@$SERVICES_IP 'docker-compose logs -f backend'"
    echo ""
    echo "5. Test end-to-end (after DNS propagates):"
    echo "   curl http://api.yourdomain.com:5000/health"
    echo ""
    echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
}

# Main deployment flow
main() {
    echo ""
    echo "╔════════════════════════════════════════════════════╗"
    echo "║        YoLab Deployment Script v1.0                ║"
    echo "║  Deploy FRPS + Services to Hetzner Cloud           ║"
    echo "╚════════════════════════════════════════════════════╝"
    echo ""
    
    check_prerequisites
    check_tfvars
    init_terraform
    show_plan
    deploy
    post_deployment_checks
    show_dns_config
    show_next_steps
    
    echo ""
    success "Deployment completed successfully!"
    echo ""
}

# Run main function
main
