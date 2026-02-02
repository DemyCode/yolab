output "server_name" {
  description = "Name of the YoLab server"
  value       = hcloud_server.yolab.name
}

output "server_id" {
  description = "ID of the YoLab server"
  value       = hcloud_server.yolab.id
}

output "ssh_key_id" {
  description = "ID of the SSH key"
  value       = data.hcloud_ssh_key.deployment_key.id
}

output "ssh_key_fingerprint" {
  description = "Fingerprint of the SSH key"
  value       = data.hcloud_ssh_key.deployment_key.fingerprint
}

output "server_ipv4" {
  description = "IPv4 address of the YoLab server"
  value       = hcloud_server.yolab.ipv4_address
}

output "server_ipv6" {
  description = "IPv6 address of the YoLab server"
  value       = hcloud_server.yolab.ipv6_address
}

output "ssh_command" {
  description = "SSH command to access server"
  value       = "ssh root@${hcloud_server.yolab.ipv4_address}"
}

output "frps_url" {
  description = "FRP server control endpoint"
  value       = "http://[${hcloud_server.yolab.ipv6_address}]:7000"
}

output "backend_api_url" {
  description = "Backend API URL"
  value       = "http://${hcloud_server.yolab.ipv4_address}:5000"
}

output "health_check_command" {
  description = "Command to check backend health"
  value       = "curl -f http://${hcloud_server.yolab.ipv4_address}:5000/health"
}

output "dns_configuration" {
  description = "DNS records to configure"
  value = {
    domain = var.domain
    records = [
      {
        type  = "A"
        name  = "@"
        value = hcloud_server.yolab.ipv4_address
      },
      {
        type  = "AAAA"
        name  = "@"
        value = hcloud_server.yolab.ipv6_address
      },
      {
        type  = "A"
        name  = "*"
        value = hcloud_server.yolab.ipv4_address
      },
      {
        type  = "AAAA"
        name  = "*"
        value = hcloud_server.yolab.ipv6_address
      },
      {
        type  = "NS"
        name  = "@"
        value = "ns1.${var.domain}"
      },
      {
        type  = "A"
        name  = "ns1"
        value = hcloud_server.yolab.ipv4_address
      },
      {
        type  = "AAAA"
        name  = "ns1"
        value = hcloud_server.yolab.ipv6_address
      }
    ]
  }
}
