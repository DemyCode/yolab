# YoLab - IPv6 Tunneling Service Infrastructure

A complete, production-ready infrastructure for running an IPv6 tunneling service using FRP (Fast Reverse Proxy). This monorepo contains everything needed to deploy and manage a service that allows users to expose their local services through IPv6 addresses and custom subdomains.

## Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                        Client Services                          │
│              (Users running FRP clients locally)                │
└───────────────────────────┬─────────────────────────────────────┘
                            │
                            │ FRP Connection
                            ▼
┌─────────────────────────────────────────────────────────────────┐
│                       YoLab Platform                            │
│                                                                 │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐        │
│  │   Backend    │  │  DNS Server  │  │   FRP Server │        │
│  │   (FastAPI)  │  │   (Python)   │  │   (Homelab)  │        │
│  │              │  │              │  │              │        │
│  │ Registration │  │   Subdomain  │  │   Tunneling  │        │
│  │   & Config   │  │  Resolution  │  │     Proxy    │        │
│  └──────────────┘  └──────────────┘  └──────────────┘        │
│         │                  │                  │                │
│         └──────────────────┴──────────────────┘                │
│                         PostgreSQL                              │
└─────────────────────────────────────────────────────────────────┘
                            │
                            │ AAAA Records
                            ▼
                     Public IPv6 Access
```

## Features

- **Automatic IPv6 Allocation**: Each service gets a unique IPv6 address
- **Custom Subdomains**: Users can choose their own subdomain (e.g., `myapp.yourdomain.com`)
- **Service Templates**: Pre-configured templates for common services (Home Assistant, Nextcloud, etc.)
- **Dynamic DNS**: Custom DNS server with automatic subdomain resolution
- **FRP Authentication**: Integrated auth plugin for secure tunnel management
- **RESTful API**: Complete API for service registration and management
- **Type-Safe**: Full type checking with mypy/pyright
- **Modern Stack**: FastAPI, SQLModel, NixOS, uv package manager
- **CI/CD Ready**: Pre-commit hooks and GitHub Actions workflows
- **Bootable ISO**: Automated builds of NixOS installer images

## Repository Structure

```
yolab/
├── backend/                # FastAPI registration service
│   ├── backend/           # Main application package
│   │   ├── routes/        # API endpoints
│   │   ├── models.py      # SQLModel database models
│   │   ├── schemas.py     # Pydantic request/response schemas
│   │   ├── utils.py       # Helper functions
│   │   └── settings.py    # Configuration management
│   ├── alembic/           # Database migrations
│   ├── services/          # Service template configurations
│   └── Dockerfile         # Container image
│
├── dns_server/            # Custom DNS server
│   ├── dns_server/        # DNS implementation
│   │   ├── server.py      # DNS server logic
│   │   └── resolver.py    # Backend API integration
│   └── Dockerfile         # Container image
│
├── homelab/               # NixOS infrastructure
│   ├── configuration.nix  # Main system configuration
│   ├── disk-config.nix    # Disko disk partitioning
│   ├── flake.nix          # Nix flake for reproducible builds
│   ├── modules/           # Custom NixOS modules
│   │   ├── frpc.nix       # FRP server configuration
│   │   └── homelab-setup.nix
│   └── installer/         # ISO installer configuration
│
└── .github/
    └── workflows/         # CI/CD pipelines
        ├── pre-commit.yml # Code quality checks
        └── build-iso.yml  # NixOS ISO builds
```

## Components

### Backend (FastAPI)

The registration API handles:
- User account creation (via account tokens)
- Service registration and configuration
- IPv6 address allocation
- FRP authentication plugin
- Service template management
- Statistics and monitoring

**Key Endpoints:**
- `POST /generate-token` - Create new account token
- `POST /register` - Register a new service
- `GET /dashboard/{token}` - View all user services
- `GET /service/{id}/config` - Get FRP configuration
- `GET /templates` - List available service templates

### DNS Server

Custom DNS server that:
- Resolves subdomains to IPv6 addresses dynamically
- Queries backend API for active services
- Falls back to main server for unknown subdomains
- Supports AAAA record queries

### Homelab (NixOS)

Declarative infrastructure configuration:
- FRP server setup with custom authentication
- Automatic system configuration with Disko
- Network and firewall rules
- Service monitoring and health checks
- Reproducible builds via Nix flakes

## Quick Start

### Prerequisites

- Python 3.11+
- [uv](https://github.com/astral-sh/uv) package manager
- Docker (optional, for containerized deployment)
- PostgreSQL database
- NixOS (for homelab deployment)

### Backend Setup

```bash
cd backend

# Install dependencies
uv sync

# Configure environment
cp .env.example .env
# Edit .env with your configuration

# Run database migrations
uv run alembic upgrade head

# Start the server
uv run python -m backend.main
```

The API will be available at `http://localhost:5000`

### DNS Server Setup

```bash
cd dns_server

# Install dependencies
uv sync

# Configure environment
cp .env.example .env
# Edit .env with backend URL

# Start the DNS server
uv run python -m dns_server.server
```

The DNS server will listen on port 53 (requires root or CAP_NET_BIND_SERVICE)

### Homelab Setup

```bash
cd homelab

# Build the NixOS configuration
nix build .#nixosConfigurations.homelab.config.system.build.toplevel

# Or build the installer ISO
nix build .#iso

# Deploy to target machine
nixos-rebuild switch --flake .#homelab
```

## Development

### Code Quality

This project uses pre-commit hooks for code quality:

```bash
# Install pre-commit
uv tool run pre-commit install

# Run all checks
uv tool run pre-commit run --all-files
```

Hooks include:
- Ruff (formatting and linting)
- Type checking (mypy/pyright)
- Trailing whitespace removal
- YAML validation
- Dependency synchronization

### Database Migrations

```bash
cd backend

# Create a new migration
uv run alembic revision --autogenerate -m "description"

# Apply migrations
uv run alembic upgrade head

# Rollback one migration
uv run alembic downgrade -1
```

### Adding Service Templates

1. Create a directory in `backend/services/{service-name}/`
2. Add `docker-compose.yml` for service setup
3. Add `Caddyfile` for reverse proxy configuration
4. Template variables available: `{{DOMAIN}}`, `{{IPV6}}`, `{{PORT}}`

## Deployment

### Docker Compose

```bash
# Build and start all services
docker-compose up -d

# View logs
docker-compose logs -f

# Stop services
docker-compose down
```

### NixOS (Recommended)

The homelab configuration provides a complete, declarative deployment:

```bash
# Deploy from the ISO installer
# Boot from USB, then:
curl http://localhost:8000/detect  # Detect disks
curl -X POST http://localhost:8000/install \
  -H "Content-Type: application/json" \
  -d '{
    "disk": "/dev/sda",
    "hostname": "yolab-prod",
    "root_ssh_key": "ssh-ed25519 AAAA..."
  }'
```

The installer will:
1. Partition the disk with Disko
2. Install NixOS with the homelab configuration
3. Set up FRP server, database, and all services
4. Configure networking and firewall
5. Enable automatic updates

## API Usage

### Register a New Service

```bash
# Get an account token
TOKEN=$(curl -X POST http://api.yourdomain.com/generate-token | jq -r .account_token)

# Register a service
curl -X POST http://api.yourdomain.com/register \
  -H "Content-Type: application/json" \
  -d '{
    "account_token": "'$TOKEN'",
    "service_name": "my-webapp",
    "service_type": "tcp",
    "subdomain": "myapp",
    "local_port": 8080,
    "remote_port": 443
  }'
```

### Get FRP Client Configuration

```bash
# Get the FRP config for your service
SERVICE_ID=123
curl http://api.yourdomain.com/service/$SERVICE_ID/config | jq -r .frpc_config > frpc.ini

# Start FRP client
frpc -c frpc.ini
```

Your service is now accessible at `https://myapp.yourdomain.com`

## Configuration

### Backend Environment Variables

```env
DOMAIN=yourdomain.com
FRPS_SERVER_IPV6=2001:db8::1
FRPS_SERVER_PORT=7000
IPV6_SUBNET_BASE=2001:db8:1000::
DATABASE_URL=postgresql://user:pass@localhost/yolab
REGISTRATION_API_HOST=0.0.0.0
REGISTRATION_API_PORT=5000
```

### DNS Server Environment Variables

```env
BACKEND_API_URL=http://localhost:5000
DNS_PORT=53
DNS_HOST=0.0.0.0
```

## Monitoring

### Health Checks

```bash
# Backend health
curl http://localhost:5000/health

# View statistics
curl http://localhost:5000/stats

# Check service status
curl http://localhost:5000/dashboard/{account_token}
```

### Logs

```bash
# Backend logs
docker-compose logs -f backend

# DNS server logs
docker-compose logs -f dns_server

# NixOS system logs
journalctl -u frps -f
```

## Contributing

1. Fork the repository
2. Create a feature branch (`git checkout -b feature/amazing-feature`)
3. Make your changes
4. Run pre-commit hooks (`pre-commit run --all-files`)
5. Commit your changes (`git commit -m 'Add amazing feature'`)
6. Push to the branch (`git push origin feature/amazing-feature`)
7. Open a Pull Request

## Security

- All API tokens use cryptographically secure random generation
- FRP tunnels are authenticated via custom plugin
- Database queries use parameterized statements (SQLModel)
- Type checking prevents common bugs
- Regular security updates via NixOS

## Performance

- Async FastAPI handlers for high concurrency
- Connection pooling for database access
- Efficient IPv6 allocation with atomic counter
- DNS caching for frequently queried domains
- Pre-commit hooks prevent performance regressions

## License

This project is licensed under the MIT License - see the LICENSE file for details.

## Acknowledgments

- [FastAPI](https://fastapi.tiangolo.com/) - Modern web framework
- [FRP](https://github.com/fatedier/frp) - Fast Reverse Proxy
- [NixOS](https://nixos.org/) - Declarative system configuration
- [uv](https://github.com/astral-sh/uv) - Fast Python package manager
- [SQLModel](https://sqlmodel.tiangolo.com/) - SQL databases with Python

## Support

For issues and questions:
- Open an issue on GitHub
- Check existing documentation
- Review the API endpoints at `/docs` (Swagger UI)

---

Built with modern tools for reliable infrastructure management.
