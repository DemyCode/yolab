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

### For Server Deployment

**New: All-in-One Deployment (Recommended)**

Deploy everything to a single server in under 10 minutes:

```bash
# Using GitHub Actions (easiest)
# 1. Set up GitHub secrets (see deployment/QUICKSTART.md)
# 2. Go to Actions → Deploy YoLab Server → Run workflow

# Or using local script
cd deployment/scripts
TARGET_HOST="YOUR_IP" \
POSTGRES_PASSWORD="password" \
IPV6_SUBNET_BASE="2001:db8::1:0:0:0" \
FRPS_SERVER_IPV6="2001:db8::1" \
DOMAIN="yourdomain.com" \
./deploy-all-in-one.sh
```

**See [deployment/QUICKSTART.md](deployment/QUICKSTART.md) for fast deployment guide.**

**See [deployment/README.md](deployment/README.md) for detailed instructions.**

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

### Option 1: All-in-One Server (Recommended)

Deploy everything to a single server:
- **Components**: FRP Server + Backend + DNS + Database
- **Cost**: ~€5/month for 1x CPX11 server
- **Setup Time**: 10 minutes with GitHub Actions
- **Configuration**: `deployment/nixos/all-in-one.nix`
- **Best for**: Small to medium deployments, cost-conscious users

### Option 2: Separate Servers (Advanced)

Deploy components across two servers:
- **Server 1**: FRP Server only
- **Server 2**: Backend + DNS + Database
- **Cost**: ~€10/month for 2x CPX11 servers
- **Setup Time**: 20 minutes with Terraform
- **Best for**: High-traffic deployments, geographic distribution

### Option 3: Local Development

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

### Deployment Methods

- **GitHub Actions** (easiest): Automated via workflow
- **Deployment Script**: One-command local deployment
- **nixos-anywhere**: Direct Nix flake deployment
- **Terraform**: Infrastructure-as-code with Hetzner Cloud

**Quick Links:**
- [Quick Start Guide](deployment/QUICKSTART.md) - Deploy in 10 minutes
- [Full Deployment Guide](deployment/README.md) - Detailed instructions
- [GitHub Actions Setup](#github-actions-deployment) - CI/CD deployment

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
