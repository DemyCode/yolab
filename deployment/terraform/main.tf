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

# Look up existing SSH key by name
# If it doesn't exist, this will fail with a clear error
data "hcloud_ssh_key" "deployment_key" {
  name = var.ssh_key_name
}

resource "hcloud_server" "frps_server" {
  name        = "yolab-frps"
  server_type = var.frps_server_type
  location    = var.hetzner_location
  image       = "ubuntu-22.04"
  ssh_keys    = [data.hcloud_ssh_key.deployment_key.id]

  public_net {
    ipv4_enabled = true
    ipv6_enabled = true
  }

  labels = {
    role        = "frps"
    environment = var.environment
  }

  lifecycle {
    ignore_changes = [image]
  }
}

resource "hcloud_server" "services_stack" {
  name        = "yolab-services"
  server_type = var.services_server_type
  location    = var.hetzner_location
  image       = "ubuntu-22.04"
  ssh_keys    = [data.hcloud_ssh_key.deployment_key.id]

  public_net {
    ipv4_enabled = true
    ipv6_enabled = true
  }

  labels = {
    role        = "services"
    environment = var.environment
  }

  lifecycle {
    ignore_changes = [image]
  }
}

resource "local_file" "ssh_public_key" {
  content  = var.ssh_public_key
  filename = "${path.module}/.ssh_public_key.tmp"
}

resource "local_file" "frps_deployment_config" {
  filename = "${path.module}/../nixos/ignored/config-frps.json"

  content = jsonencode({
    server = {
      hostname = "yolab-frps"
      domain   = var.domain
      repo_url = var.repo_url
    }
    ssh = {
      public_key = var.ssh_public_key
      key_name   = var.ssh_key_name
    }
    network = {
      frps_server_ipv4 = hcloud_server.frps_server.ipv4_address
      frps_bind_port   = 7000
      auth_plugin_addr = "${hcloud_server.services_stack.ipv4_address}:5000"
    }
    frps = {
      enable    = true
      bind_port = 7000
    }
    services = {
      enable = false
    }
    nftables = {
      enable         = true
      nftables_file = "/var/lib/nftables-manager/rules.nft"
      log_level      = "DEBUG"
    }
  })

  file_permission = "0644"
}

resource "local_file" "services_deployment_config" {
  filename = "${path.module}/../nixos/ignored/config-services.json"

  content = jsonencode({
    server = {
      domain   = var.domain
      repo_url = var.repo_url
    }
    ssh = {
      public_key = var.ssh_public_key
      key_name   = var.ssh_key_name
    }
    database = {
      db_name     = var.postgres_db
      db_user     = var.postgres_user
      db_password = var.postgres_password
    }
    network = {
      ipv6_subnet_base = "${hcloud_server.services_stack.ipv6_address}1"
      frps_server_ipv4 = hcloud_server.frps_server.ipv4_address
      frps_bind_port   = 7000
    }
    frps = {
      enable = false
    }
    services = {
      enable        = true
      api_host      = "0.0.0.0"
      api_port      = 5000
      auto_update   = true
      open_firewall = true
    }
  })

  file_permission = "0644"
}

module "deploy_nixos_frps" {
  source = "github.com/nix-community/nixos-anywhere//terraform/all-in-one"

  nixos_system_attr      = "path:${path.module}/../..#nixosConfigurations.frps-server.config.system.build.toplevel"
  nixos_partitioner_attr = "path:${path.module}/../..#nixosConfigurations.frps-server.config.system.build.diskoScript"
  target_host            = hcloud_server.frps_server.ipv4_address
  instance_id            = hcloud_server.frps_server.id
  install_ssh_key        = var.ssh_private_key
  deployment_ssh_key     = var.ssh_private_key

  depends_on = [local_file.frps_deployment_config]
}

module "deploy_nixos_services" {
  source = "github.com/nix-community/nixos-anywhere//terraform/all-in-one"

  nixos_system_attr      = "path:${path.module}/../..#nixosConfigurations.services-stack.config.system.build.toplevel"
  nixos_partitioner_attr = "path:${path.module}/../..#nixosConfigurations.services-stack.config.system.build.diskoScript"
  target_host            = hcloud_server.services_stack.ipv4_address
  instance_id            = hcloud_server.services_stack.id
  install_ssh_key        = var.ssh_private_key
  deployment_ssh_key     = var.ssh_private_key

  depends_on = [local_file.services_deployment_config]
}
