terraform {
  required_providers {
    hcloud = {
      source  = "hetznercloud/hcloud"
      version = "~> 1.45"
    }
  }
  required_version = ">= 1.0"
}

provider "hcloud" {
  token = var.hcloud_token
}

data "hcloud_ssh_key" "deployment_key" {
  name = var.ssh_key_name
}

resource "hcloud_server" "yolab" {
  name        = "yolab-server"
  server_type = var.server_type
  location    = var.hetzner_location
  image       = "ubuntu-22.04"
  ssh_keys    = [data.hcloud_ssh_key.deployment_key.id]

  public_net {
    ipv4_enabled = true
    ipv6_enabled = true
  }

  labels = {
    role        = "all-in-one"
    environment = var.environment
  }

  lifecycle {
    ignore_changes = [image]
  }
}

resource "null_resource" "prepare_config" {
  triggers = {
    server_ipv6 = hcloud_server.yolab.ipv6_address
  }

  provisioner "local-exec" {
    command = <<-EOT
      cd ${path.module}/../..
      sed -e "s|REPLACE_REPO_URL|${var.repo_url}|g" \
          -e "s|REPLACE_DOMAIN|${var.domain}|g" \
          -e "s|REPLACE_POSTGRES_DB|${var.postgres_db}|g" \
          -e "s|REPLACE_POSTGRES_USER|${var.postgres_user}|g" \
          -e "s|REPLACE_POSTGRES_PASSWORD|${var.postgres_password}|g" \
          -e "s|REPLACE_IPV6_SUBNET_BASE|${var.ipv6_subnet_base}|g" \
          -e "s|REPLACE_FRPS_SERVER_IPV6|${hcloud_server.yolab.ipv6_address}|g" \
          deployment/nixos/all-in-one.nix > deployment/nixos/all-in-one.nix.tmp
      mv deployment/nixos/all-in-one.nix.tmp deployment/nixos/all-in-one.nix
    EOT
  }
}

module "deploy_nixos" {
  source = "github.com/nix-community/nixos-anywhere//terraform/all-in-one"

  nixos_system_attr      = ".#nixosConfigurations.yolab-server.config.system.build.toplevel"
  nixos_partitioner_attr = ".#nixosConfigurations.yolab-server.config.system.build.diskoScript"
  target_host            = hcloud_server.yolab.ipv4_address
  instance_id            = hcloud_server.yolab.id

  depends_on = [null_resource.prepare_config]
}
