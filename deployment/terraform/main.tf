terraform {
  required_providers {
    hcloud = {
      source  = "hetznercloud/hcloud"
      version = "~> 1.45"
    }
    filemanager = {
      source  = "ebogdum/filemanager"
      version = "~> 1.2"
    }
  }
  required_version = ">= 1.0"
}

provider "hcloud" {
  token = var.hcloud_token
}

provider "filemanager" {}

resource "hcloud_ssh_key" "deployment_key" {
  name       = var.ssh_key_name
  public_key = var.ssh_public_key
}

resource "hcloud_server" "yolab" {
  name        = "yolab-server"
  server_type = var.server_type
  location    = var.hetzner_location
  image       = "ubuntu-22.04"
  ssh_keys    = [hcloud_ssh_key.deployment_key.id]

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

resource "local_file" "ssh_public_key" {
  content  = var.ssh_public_key
  filename = "${path.module}/.ssh_public_key.tmp"
}

resource "filemanager_toml_file" "deployment_config" {
  path = "${path.module}/../nixos/ignored/config.toml"

  content = {
    server = {
      hostname = "yolab-server"
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
      # Auto-calculate subnet base from server's IPv6
      # Server gets: 2a01:4f8:c010:1234::1
      # We use:      2a01:4f8:c010:1234::1:0:0:0 for client allocation
      ipv6_subnet_base = "${hcloud_server.yolab.ipv6_address}:1:0:0:0"
      frps_server_ipv6 = hcloud_server.yolab.ipv6_address
      frps_bind_port   = 7000
      auth_plugin_addr = "127.0.0.1:5000"
    }

    frps = {
      enable = true
    }

    services = {
      enable        = true
      api_host      = "0.0.0.0"
      api_port      = 5000
      auto_update   = true
      open_firewall = true
    }
  }
}

module "deploy_nixos" {
  source = "github.com/nix-community/nixos-anywhere//terraform/all-in-one"

  nixos_system_attr      = ".#nixosConfigurations.yolab-server.config.system.build.toplevel"
  nixos_partitioner_attr = ".#nixosConfigurations.yolab-server.config.system.build.diskoScript"
  target_host            = hcloud_server.yolab.ipv4_address
  instance_id            = hcloud_server.yolab.id
  install_ssh_key        = local_file.ssh_public_key.filename

  depends_on = [filemanager_toml_file.deployment_config]
}

