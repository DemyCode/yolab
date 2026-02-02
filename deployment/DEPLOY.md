# YoLab All-in-One Deployment

## Structure

```
deployment/nixos/
├── modules/
│   ├── frps.nix          - FRP server module
│   └── services.nix      - Services stack module
├── all-in-one.nix        - Combines both modules
├── frps-server.nix       - Only FRP server
└── services-stack.nix    - Only services
```

## Deploy

### GitHub Actions

1. Set secrets: DEPLOY_SSH_KEY, POSTGRES_PASSWORD, IPV6_SUBNET_BASE, FRPS_SERVER_IPV6, POSTGRES_DB, POSTGRES_USER
2. Actions → Deploy YoLab Server → Run workflow

### Local

```bash
TARGET_HOST="192.168.1.100" \
POSTGRES_PASSWORD="pass" \
IPV6_SUBNET_BASE="2001:db8::1:0:0:0" \
FRPS_SERVER_IPV6="2001:db8::1" \
DOMAIN="domain.com" \
./deployment/scripts/deploy-all-in-one.sh
```

## Module Options

### services.yolab-frps
- enable
- domain
- authPluginAddr (default: 127.0.0.1:5000)
- bindPort (default: 7000)
- openFirewall (default: true)

### services.yolab-services
- enable
- repoUrl
- domain
- postgresDb (default: frp_services)
- postgresUser (default: frp_user)
- postgresPassword
- ipv6SubnetBase
- frpsServerIpv6
- openFirewall (default: true)
- autoUpdate (default: true)
