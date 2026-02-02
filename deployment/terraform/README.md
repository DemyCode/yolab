# YoLab Terraform Deployment

Deploy YoLab all-in-one server to Hetzner Cloud using Terraform and nixos-anywhere.

## Prerequisites

1. **Hetzner Cloud Account**
   - Sign up at https://console.hetzner.cloud/
   - Generate API token (Project → Security → API Tokens)

2. **SSH Key**
   - Upload your SSH public key to Hetzner Cloud
   - Go to: Project → Security → SSH Keys
   - Note the name you give it

3. **Domain**
   - Register a domain
   - You'll need to configure DNS records after deployment

## GitHub Secrets Setup

Configure these secrets in your GitHub repository (Settings → Secrets → Actions):

### Required Secrets:
- `HCLOUD_TOKEN` - Your Hetzner Cloud API token
- `SSH_KEY_NAME` - Name of SSH key in Hetzner Cloud (e.g., "deployment-key")
- `POSTGRES_PASSWORD` - PostgreSQL password (generate strong password)
- `IPV6_SUBNET_BASE` - IPv6 subnet for client allocation (get from Hetzner server IPv6)

### Optional Secrets:
- `POSTGRES_DB` - Database name (default: frp_services)
- `POSTGRES_USER` - Database user (default: frp_user)
- `SERVER_TYPE` - Hetzner server type (default: cpx11)
- `HETZNER_LOCATION` - Datacenter location (default: nbg1)

## Deployment via GitHub Actions

1. Go to your GitHub repository
2. Click "Actions" tab
3. Select "Deploy YoLab with Terraform"
4. Click "Run workflow"
5. Enter your domain name
6. Choose action: "apply" (to deploy) or "destroy" (to delete)
7. Wait 5-10 minutes for deployment to complete

## Manual Local Deployment

### 1. Install Requirements
```bash
nix-shell -p terraform_1
```

### 2. Configure Variables
```bash
cd deployment/terraform
cp terraform.tfvars.example terraform.tfvars
```

Edit `terraform.tfvars` with your values.

### 3. Initialize Terraform
```bash
terraform init
```

### 4. Plan Deployment
```bash
terraform plan
```

### 5. Deploy
```bash
terraform apply
```

### 6. Get Server Info
```bash
terraform output
```

## After Deployment

### 1. Configure DNS Records

Point your domain to the deployed server:

```
yourdomain.com      A      <server-ipv4>
yourdomain.com      AAAA   <server-ipv6>
*.yourdomain.com    A      <server-ipv4>
*.yourdomain.com    AAAA   <server-ipv6>
ns1.yourdomain.com  A      <server-ipv4>
ns1.yourdomain.com  AAAA   <server-ipv6>
yourdomain.com      NS     ns1.yourdomain.com
```

### 2. Test Deployment

```bash
ssh root@<server-ipv4>

curl http://<server-ipv4>:5000/health

curl -X POST http://<server-ipv4>:5000/api/token/new
```

### 3. Monitor Services

```bash
ssh root@<server-ipv4>

systemctl status frps
systemctl status yolab-deploy

docker ps
docker logs frp-backend
```

## Destroy Infrastructure

To delete everything:

### Via GitHub Actions:
1. Go to Actions → Deploy YoLab with Terraform
2. Run workflow with action: "destroy"

### Manually:
```bash
cd deployment/terraform
terraform destroy
```

## Troubleshooting

### Issue: nixos-anywhere fails
- Check SSH key is uploaded to Hetzner
- Verify server can boot (check Hetzner console)
- Check nixos-anywhere logs in GitHub Actions

### Issue: Health check fails
- SSH to server and check logs: `docker logs frp-backend`
- Check services: `systemctl status yolab-deploy`
- Verify environment variables in `/opt/yolab/repo/.env`

### Issue: Terraform state conflicts
- Terraform state is stored locally in GitHub Actions
- For production, use Terraform Cloud or S3 backend

## Server Specifications

Default: `cpx11` (€4.85/month)
- 2 vCPU
- 2 GB RAM
- 40 GB SSD
- 20 TB traffic

To change, set `SERVER_TYPE` secret to: cpx21, cpx31, cpx41, etc.

## Locations

Default: `nbg1` (Nuremberg, Germany)

Available locations:
- `nbg1` - Nuremberg
- `fsn1` - Falkenstein
- `hel1` - Helsinki
- `ash` - Ashburn, VA
- `hil` - Hillsboro, OR

## Cost Estimate

- Server: €4.85/month (cpx11)
- Traffic: Included (20TB)
- IPv6: Free

**Total: ~€5/month**
