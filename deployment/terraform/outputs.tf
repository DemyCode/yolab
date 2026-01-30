output "frps_server_name" {
  description = "Name of the FRPS server"
  value       = hcloud_server.frps.name
}

output "frps_server_id" {
  description = "ID of the FRPS server"
  value       = hcloud_server.frps.id
}

output "frps_server_ipv4" {
  description = "IPv4 address of the FRPS server"
  value       = hcloud_server.frps.ipv4_address
}

output "frps_server_ipv6" {
  description = "IPv6 address of the FRPS server"
  value       = hcloud_server.frps.ipv6_address
}

output "services_server_name" {
  description = "Name of the services stack server"
  value       = hcloud_server.services.name
}

output "services_server_id" {
  description = "ID of the services stack server"
  value       = hcloud_server.services.id
}

output "services_server_ipv4" {
  description = "IPv4 address of the services stack server"
  value       = hcloud_server.services.ipv4_address
}

output "services_server_ipv6" {
  description = "IPv6 address of the services stack server"
  value       = hcloud_server.services.ipv6_address
}

output "ssh_commands" {
  description = "SSH commands to access servers"
  value = {
    frps     = "ssh root@${hcloud_server.frps.ipv4_address}"
    services = "ssh root@${hcloud_server.services.ipv4_address}"
  }
}

output "dns_configuration" {
  description = "DNS records to configure"
  value = {
    domain = var.domain
    records = [
      {
        type  = "A"
        name  = "@"
        value = hcloud_server.frps.ipv4_address
      },
      {
        type  = "AAAA"
        name  = "@"
        value = hcloud_server.frps.ipv6_address
      },
      {
        type  = "A"
        name  = "*.${var.domain}"
        value = hcloud_server.frps.ipv4_address
      },
      {
        type  = "AAAA"
        name  = "*.${var.domain}"
        value = hcloud_server.frps.ipv6_address
      },
      {
        type  = "A"
        name  = "api"
        value = hcloud_server.services.ipv4_address
      },
      {
        type  = "AAAA"
        name  = "api"
        value = hcloud_server.services.ipv6_address
      }
    ]
  }
}

output "deployment_info" {
  description = "Deployment information"
  value = {
    frps_url     = "http://${hcloud_server.frps.ipv4_address}:7000"
    api_url      = "http://${hcloud_server.services.ipv4_address}:5000"
    health_check = "curl http://${hcloud_server.services.ipv4_address}:5000/health"
  }
}
