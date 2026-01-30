variable "hcloud_token" {
  description = "Hetzner Cloud API token"
  type        = string
  sensitive   = true
}

variable "ssh_key_name" {
  description = "Name of the SSH key in Hetzner Cloud"
  type        = string
}

variable "github_repo" {
  description = "GitHub repository in format: owner/repo"
  type        = string
  default     = "your-username/yolab"
}

variable "repo_url" {
  description = "Full repository URL for cloning"
  type        = string
  default     = "https://github.com/your-username/yolab.git"
}

variable "hetzner_location" {
  description = "Hetzner datacenter location"
  type        = string
  default     = "nbg1"  # Nuremberg
}

variable "frps_server_type" {
  description = "Server type for FRPS server"
  type        = string
  default     = "cpx11"  # 2 vCPU, 2GB RAM
}

variable "services_server_type" {
  description = "Server type for services stack"
  type        = string
  default     = "cpx11"  # 2 vCPU, 2GB RAM
}

variable "environment" {
  description = "Environment name (e.g., production, staging)"
  type        = string
  default     = "production"
}

# Application configuration
variable "domain" {
  description = "Domain name for the service"
  type        = string
}

variable "ipv6_subnet_base" {
  description = "IPv6 subnet base for client allocation"
  type        = string
}

# Database configuration
variable "postgres_db" {
  description = "PostgreSQL database name"
  type        = string
  default     = "frp_services"
}

variable "postgres_user" {
  description = "PostgreSQL user"
  type        = string
  default     = "frp_user"
}

variable "postgres_password" {
  description = "PostgreSQL password"
  type        = string
  sensitive   = true
}
