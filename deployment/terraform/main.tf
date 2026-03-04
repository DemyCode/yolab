terraform {
  required_providers {
    hcloud = {
      source  = "hetznercloud/hcloud"
      version = "~> 1.45"
    }
    wireguard = {
      source  = "OJFord/wireguard"
      version = "~> 0.3"
    }
    random = {
      source  = "hashicorp/random"
      version = "~> 3.6"
    }
  }
  required_version = ">= 1.0"
}

provider "hcloud" {
  token = var.hcloud_token
}

locals {
  wg_ipv6_prefix = trimsuffix(hcloud_server.wireguard_server.ipv6_address, "1")
  wg_server_ipv6 = "${local.wg_ipv6_prefix}11"
  wg_peers_start = "${local.wg_ipv6_prefix}12"
  wg_endpoint    = "${hcloud_server.wireguard_server.ipv4_address}:51820"
}

resource "wireguard_asymmetric_key" "server" {}

resource "random_password" "postgres" {
  length  = 32
  special = false
}

data "hcloud_ssh_key" "deployment_key" {
  name = var.ssh_key_name
}

resource "hcloud_server" "wireguard_server" {
  name        = "yolab-wireguard"
  server_type = var.wg_server_type
  location    = var.hetzner_location
  image       = "ubuntu-22.04"
  ssh_keys    = [data.hcloud_ssh_key.deployment_key.id]

  public_net {
    ipv4_enabled = true
    ipv6_enabled = true
  }

  labels = {
    role        = "wireguard"
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

resource "local_file" "wireguard_deployment_config" {
  filename = "${path.module}/../nixos/ignored/config-wireguard.json"

  content = jsonencode({
    server = {
      hostname = "yolab-wireguard"
      domain   = var.domain
    }
    ssh = {
      public_key = var.ssh_public_key
      key_name   = var.ssh_key_name
    }
    network = {
      ipv6_address = hcloud_server.wireguard_server.ipv6_address
      backend_url  = "${hcloud_server.services_stack.ipv4_address}:5000"
    }
    wireguard = {
      enable      = true
      interface   = "wg0"
      address     = "${local.wg_server_ipv6}/64"
      listen_port = 51820
      private_key = wireguard_asymmetric_key.server.private_key
    }
    wireguard_manager = {
      enable        = true
      poll_interval = 30
    }
  })

  file_permission = "0600"
}

resource "local_file" "services_deployment_config" {
  filename = "${path.module}/../nixos/ignored/config-services.json"

  content = jsonencode({
    server = {
      domain = var.domain
    }
    ssh = {
      public_key = var.ssh_public_key
      key_name   = var.ssh_key_name
    }
    database = {
      db_name     = "yolab"
      db_user     = "yolab"
      db_password = random_password.postgres.result
    }
    network = {
      ipv6_subnet_base     = local.wg_peers_start
      wg_server_endpoint   = local.wg_endpoint
      wg_server_public_key = wireguard_asymmetric_key.server.public_key
      wg_server_ipv6       = hcloud_server.wireguard_server.ipv6_address
    }
    services = {
      enable        = true
      api_host      = "0.0.0.0"
      api_port      = 5000
      auto_update   = true
      open_firewall = true
    }
  })

  file_permission = "0600"
}

module "deploy_nixos_wireguard_server" {
  source = "github.com/nix-community/nixos-anywhere//terraform/all-in-one"

  nixos_system_attr      = "path:${path.module}/../..#nixosConfigurations.wireguard-server.config.system.build.toplevel"
  nixos_partitioner_attr = "path:${path.module}/../..#nixosConfigurations.wireguard-server.config.system.build.diskoScript"
  target_host            = hcloud_server.wireguard_server.ipv4_address
  instance_id            = hcloud_server.wireguard_server.id
  install_ssh_key        = var.ssh_private_key
  deployment_ssh_key     = var.ssh_private_key

  depends_on = [local_file.wireguard_deployment_config]
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
