output "wg_public_key" {
  description = "Generated WireGuard server public key"
  value       = wireguard_asymmetric_key.server.public_key
}

output "postgres_password" {
  description = "Generated PostgreSQL password"
  value       = random_password.postgres.result
  sensitive   = true
}

output "wireguard_server_name" {
  description = "Name of the WireGuard server"
  value       = hcloud_server.wireguard_server.name
}

output "wireguard_server_id" {
  description = "ID of the WireGuard server"
  value       = hcloud_server.wireguard_server.id
}

output "wireguard_server_ipv4" {
  description = "IPv4 address of the WireGuard server"
  value       = hcloud_server.wireguard_server.ipv4_address
}

output "wireguard_server_ipv6" {
  description = "IPv6 address of the WireGuard server"
  value       = hcloud_server.wireguard_server.ipv6_address
}

output "wireguard_server_ssh" {
  description = "SSH command to access WireGuard server"
  value       = "ssh root@${hcloud_server.wireguard_server.ipv4_address}"
}

output "wireguard_endpoint" {
  description = "WireGuard endpoint for client wg0.conf"
  value       = "${hcloud_server.wireguard_server.ipv4_address}:51820"
}

output "services_server_name" {
  description = "Name of the services stack server"
  value       = hcloud_server.services_stack.name
}

output "services_server_id" {
  description = "ID of the services stack server"
  value       = hcloud_server.services_stack.id
}

output "services_server_ipv4" {
  description = "IPv4 address of the services stack server"
  value       = hcloud_server.services_stack.ipv4_address
}

output "services_server_ipv6" {
  description = "IPv6 address of the services stack server"
  value       = hcloud_server.services_stack.ipv6_address
}

output "services_server_ssh" {
  description = "SSH command to access services stack server"
  value       = "ssh root@${hcloud_server.services_stack.ipv4_address}"
}

output "backend_api_url" {
  description = "Backend API URL"
  value       = "http://${hcloud_server.services_stack.ipv4_address}:5000"
}

output "ssh_key_id" {
  description = "ID of the SSH key"
  value       = data.hcloud_ssh_key.deployment_key.id
}

output "ssh_key_fingerprint" {
  description = "Fingerprint of the SSH key"
  value       = data.hcloud_ssh_key.deployment_key.fingerprint
}

output "deployment_summary" {
  description = "Summary of the deployment"
  value = {
    wireguard_server = {
      name     = hcloud_server.wireguard_server.name
      ipv4     = hcloud_server.wireguard_server.ipv4_address
      ipv6     = hcloud_server.wireguard_server.ipv6_address
      endpoint = "${hcloud_server.wireguard_server.ipv4_address}:51820"
      ssh      = "ssh root@${hcloud_server.wireguard_server.ipv4_address}"
    }
    services_stack = {
      name    = hcloud_server.services_stack.name
      ipv4    = hcloud_server.services_stack.ipv4_address
      ipv6    = hcloud_server.services_stack.ipv6_address
      api_url = "http://${hcloud_server.services_stack.ipv4_address}:5000"
      ssh     = "ssh root@${hcloud_server.services_stack.ipv4_address}"
    }
  }
}

output "dns_configuration" {
  description = "DNS records to configure"
  value = {
    domain  = var.domain
    records = [
      {
        type  = "A"
        name  = "@"
        value = hcloud_server.wireguard_server.ipv4_address
      },
      {
        type  = "AAAA"
        name  = "@"
        value = hcloud_server.wireguard_server.ipv6_address
      },
      {
        type  = "A"
        name  = "*"
        value = hcloud_server.wireguard_server.ipv4_address
      },
      {
        type  = "AAAA"
        name  = "*"
        value = hcloud_server.wireguard_server.ipv6_address
      },
      {
        type  = "A"
        name  = "api"
        value = hcloud_server.services_stack.ipv4_address
      },
      {
        type  = "AAAA"
        name  = "api"
        value = hcloud_server.services_stack.ipv6_address
      },
      {
        type  = "A"
        name  = "ns1"
        value = hcloud_server.services_stack.ipv4_address
      },
      {
        type  = "AAAA"
        name  = "ns1"
        value = hcloud_server.services_stack.ipv6_address
      },
    ]
  }
}
