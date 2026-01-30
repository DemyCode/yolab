# YoLab - IPv6 Tunneling Platform

YoLab is a complete FRP (Fast Reverse Proxy) IPv6 tunneling service that allows users to expose their services through IPv6 addresses with dynamic DNS support.

## 🎯 Unified Flake Configuration

All NixOS configurations are now unified in a single root `flake.nix`:

```bash
# Show all available configurations
nix flake show

# Enter development shell with all tools
nix develop

# Build any configuration
nix build .#frps-server
nix build .#services-stack
nix build .#yolab-client
nix build .#iso
```

**See [FLAKE_REFERENCE.md](FLAKE_REFERENCE.md) for complete flake documentation.**

## 🏗️ Architecture

The platform consists of:

- **Backend API** (FastAPI + PostgreSQL) - User registration and service management
- **Auth Plugin** - FRP client authentication via HTTP
- **DNS Server** - Dynamic DNS resolution for client subdomains
- **FRPS Server** - FRP server for handling client tunnels
- **Client Configuration** - NixOS-based homelab setup for FRP clients

## 🚀 Quick Start

### For Server Deployment (Hetzner Cloud)

Deploy the complete backend infrastructure to Hetzner Cloud:

```bash
# 1. Configure deployment
cd deployment/terraform
cp terraform.tfvars.example terraform.tfvars
vim terraform.tfvars  # Add your Hetzner API token and settings

# 2. Deploy using the helper script
cd ..
./scripts/deploy.sh

# Or manually with Terraform
cd terraform
terraform init
terraform plan
terraform apply
```

**See [deployment/README.md](deployment/README.md) for detailed deployment instructions.**

### For Client Setup (Homelab)

Set up a NixOS-based client to connect to your YoLab service:

```bash
cd homelab
cp ignored/config.toml.example ignored/config.toml
vim ignored/config.toml  # Configure your settings

# Deploy to local machine
sudo nixos-rebuild switch --flake .#yolab-client
```

**See [homelab/README.md](homelab/README.md) for client setup instructions.**

## 📁 Project Structure

```
yolab/
├── backend/              # FastAPI backend + auth plugin
│   ├── backend/          # Main application code
│   ├── alembic/          # Database migrations
│   └── Dockerfile        # Backend container
├── dns_server/           # DNS server microservice
│   ├── dns_server/       # DNS server code
│   └── Dockerfile        # DNS container
├── deployment/           # Infrastructure deployment
│   ├── terraform/        # Terraform configuration
│   ├── nixos/           # NixOS server configurations
│   ├── scripts/         # Deployment helper scripts
│   └── README.md        # Deployment guide
├── homelab/             # Client homelab setup
│   ├── nixos/           # NixOS client configuration
│   ├── installer/       # USB installer
│   └── README.md        # Client setup guide
├── config/              # FRP server configuration templates
├── docker-compose.yml   # Local development setup
└── flake.nix           # Nix flake for all configurations
```

## 🎯 Features

### Server Features
- **IPv6 Tunnel Management**: Allocate and manage IPv6 addresses for clients
- **User Authentication**: Secure token-based authentication
- **Dynamic DNS**: Automatic subdomain resolution for client services
- **Service Templates**: Pre-configured service templates (SSH, HTTP, etc.)
- **Real-time Stats**: Monitor tunnel usage and connections

### Client Features
- **Automated Setup**: NixOS-based configuration management
- **Service Management**: Easy tunnel configuration via TOML
- **Auto-updates**: Pull configuration updates from your repository
- **Web UI**: Browser-based management interface
- **Docker Support**: Run additional services via Docker Compose

## 🛠️ Technology Stack

- **Backend**: Python, FastAPI, SQLModel, PostgreSQL, Alembic
- **DNS**: Python, dnslib
- **Infrastructure**: NixOS, Terraform, Docker, Docker Compose
- **FRP**: Fast Reverse Proxy (frp)
- **Deployment**: nixos-anywhere, Hetzner Cloud

## 📦 Deployment Options

### Option 1: Full Cloud Deployment (Production)

Deploy both FRPS server and services stack to Hetzner Cloud:
- **Cost**: ~€9/month for 2x CPX11 servers
- **Setup Time**: 15-20 minutes
- **Automation**: Fully automated with Terraform

### Option 2: Local Development

Run services locally with Docker Compose:

```bash
# Create .env file
cp .env.example .env
vim .env

# Start services
docker-compose up -d

# Check health
curl http://localhost:5000/health
```

### Option 3: Hybrid Setup

- Deploy FRPS + Backend to cloud
- Run additional services locally
- Perfect for testing before full deployment

## 🔧 Configuration

### Environment Variables

For the backend services:

```bash
POSTGRES_DB=frp_services
POSTGRES_USER=frp_user
POSTGRES_PASSWORD=your_secure_password
DOMAIN=yourdomain.com
FRPS_SERVER_IPV6=2001:db8::1
IPV6_SUBNET_BASE=2001:db8::1:0:0:0
```

### DNS Records

Configure these DNS records after deployment:

```
@                   A/AAAA    <FRPS_SERVER_IP>
*.yourdomain.com    A/AAAA    <FRPS_SERVER_IP>
api                 A/AAAA    <SERVICES_SERVER_IP>
```

## 📊 Monitoring

Check service health:

```bash
# Using the health check script
./deployment/scripts/check-health.sh

# Manual checks
curl http://api.yourdomain.com:5000/health
dig @<dns-server-ip> test.clients.yourdomain.com
```

## 🧪 Development

### Prerequisites

- Nix package manager or NixOS
- Terraform (for deployment)
- Docker and Docker Compose (for local development)
- Hetzner Cloud account (for production deployment)

### Local Development Setup

```bash
# Enter development shell
nix develop

# Or install tools manually
# - terraform
# - docker
# - docker-compose
```

### Running Tests

```bash
cd backend
uv run pytest
```

## 📖 Documentation

- [Deployment Guide](deployment/README.md) - Deploy to Hetzner Cloud
- [Homelab Guide](homelab/README.md) - Client setup instructions
- [API Documentation](backend/README.md) - Backend API reference
- [DNS Server](dns_server/README.md) - DNS server documentation

## 🤝 Contributing

Contributions are welcome! Please:

1. Fork the repository
2. Create a feature branch
3. Make your changes
4. Submit a pull request

## 📝 License

[Your License Here]

## 🆘 Support

- **Issues**: [GitHub Issues](https://github.com/your-username/yolab/issues)
- **Documentation**: See `/docs` directory
- **Discussions**: [GitHub Discussions](https://github.com/your-username/yolab/discussions)

## 🎉 Acknowledgments

- [FRP](https://github.com/fatedier/frp) - Fast Reverse Proxy
- [NixOS](https://nixos.org/) - Declarative Linux distribution
- [nixos-anywhere](https://github.com/nix-community/nixos-anywhere) - Remote NixOS installation
- [Hetzner Cloud](https://www.hetzner.com/cloud) - Cloud hosting

---

**Ready to deploy?** Start with the [Deployment Guide](deployment/README.md)!
