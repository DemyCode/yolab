# =============================================================================
# FRPS Server Outputs
# =============================================================================

output "frps_server_name" {
  description = "Name of the FRPS server"
  value       = hcloud_server.frps_server.name
}

output "frps_server_id" {
  description = "ID of the FRPS server"
  value       = hcloud_server.frps_server.id
}

output "frps_server_ipv4" {
  description = "IPv4 address of the FRPS server"
  value       = hcloud_server.frps_server.ipv4_address
}

output "frps_server_ipv6" {
  description = "IPv6 address of the FRPS server"
  value       = hcloud_server.frps_server.ipv6_address
}

output "frps_ssh_command" {
  description = "SSH command to access FRPS server"
  value       = "ssh root@${hcloud_server.frps_server.ipv4_address}"
}

output "frps_server_url" {
  description = "FRP server endpoint"
  value       = "frp://[${hcloud_server.frps_server.ipv6_address}]:7000"
}

# =============================================================================
# Services Stack Outputs
# =============================================================================

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

output "services_ssh_command" {
  description = "SSH command to access services stack server"
  value       = "ssh root@${hcloud_server.services_stack.ipv4_address}"
}

output "backend_api_url" {
  description = "Backend API URL"
  value       = "http://${hcloud_server.services_stack.ipv4_address}:5000"
}

output "health_check_command" {
  description = "Command to check backend health"
  value       = "curl -f http://${hcloud_server.services_stack.ipv4_address}:5000/health"
}

# =============================================================================
# SSH Key Info
# =============================================================================

output "ssh_key_id" {
  description = "ID of the SSH key used for both servers"
  value       = data.hcloud_ssh_key.deployment_key.id
}

output "ssh_key_fingerprint" {
  description = "Fingerprint of the SSH key"
  value       = data.hcloud_ssh_key.deployment_key.fingerprint
}

# =============================================================================
# Combined Deployment Summary
# =============================================================================

output "deployment_summary" {
  description = "Summary of the two-server deployment"
  value = {
    frps_server = {
      name = hcloud_server.frps_server.name
      ipv4 = hcloud_server.frps_server.ipv4_address
      ipv6 = hcloud_server.frps_server.ipv6_address
      role = "FRP Server (tunnel connections)"
      ssh  = "ssh root@${hcloud_server.frps_server.ipv4_address}"
    }
    services_stack = {
      name    = hcloud_server.services_stack.name
      ipv4    = hcloud_server.services_stack.ipv4_address
      ipv6    = hcloud_server.services_stack.ipv6_address
      role    = "Backend API + DNS + Database"
      ssh     = "ssh root@${hcloud_server.services_stack.ipv4_address}"
      api_url = "http://${hcloud_server.services_stack.ipv4_address}:5000"
    }
    connectivity = {
      frps_to_backend = "${hcloud_server.services_stack.ipv4_address}:5000"
      status_check    = "curl http://${hcloud_server.services_stack.ipv4_address}:5000/health"
    }
  }
}

# =============================================================================
# DNS Configuration Instructions
# =============================================================================

output "dns_configuration" {
  description = "DNS records to configure manually"
  value = {
    domain       = var.domain
    instructions = "Configure these DNS records in your DNS provider:"
    records = [
      {
        comment = "Root domain points to FRPS server (main tunnel endpoint)"
        type    = "A"
        name    = "@"
        value   = hcloud_server.frps_server.ipv4_address
      },
      {
        type  = "AAAA"
        name  = "@"
        value = hcloud_server.frps_server.ipv6_address
      },
      {
        comment = "Wildcard for all client subdomains points to FRPS server"
        type    = "A"
        name    = "*"
        value   = hcloud_server.frps_server.ipv4_address
      },
      {
        type  = "AAAA"
        name  = "*"
        value = hcloud_server.frps_server.ipv6_address
      },
      {
        comment = "API endpoint points to services stack"
        type    = "A"
        name    = "api"
        value   = hcloud_server.services_stack.ipv4_address
      },
      {
        type  = "AAAA"
        name  = "api"
        value = hcloud_server.services_stack.ipv6_address
      },
      {
        comment = "DNS nameserver points to services stack"
        type    = "A"
        name    = "ns1"
        value   = hcloud_server.services_stack.ipv4_address
      },
      {
        type  = "AAAA"
        name  = "ns1"
        value = hcloud_server.services_stack.ipv6_address
      },
      {
        comment = "Optional: Nameserver delegation"
        type    = "NS"
        name    = "@"
        value   = "ns1.${var.domain}"
      }
    ]
  }
}
