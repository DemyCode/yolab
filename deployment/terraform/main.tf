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

module "frps_server" {
  source = "github.com/nix-community/nixos-anywhere//terraform/all-in-one"

  nixos_system_attr      = ".#nixosConfigurations.frps-server.config.system.build.toplevel"
  nixos_partitioner_attr = ".#nixosConfigurations.frps-server.config.system.build.diskoScript"
  target_host            = hcloud_server.frps.ipv4_address
}

module "services_stack" {
  source = "github.com/nix-community/nixos-anywhere//terraform/all-in-one"

  nixos_system_attr      = ".#nixosConfigurations.services-stack.config.system.build.toplevel"
  nixos_partitioner_attr = ".#nixosConfigurations.services-stack.config.system.build.diskoScript"
  target_host            = hcloud_server.services.ipv4_address

  depends_on = [module.frps_server]
}

resource "hcloud_server" "frps" {
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

resource "hcloud_server" "services" {
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

  depends_on = [hcloud_server.frps]
}
