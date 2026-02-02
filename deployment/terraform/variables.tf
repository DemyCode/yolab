variable "hcloud_token" {
  description = "Hetzner Cloud API token"
  type        = string
  sensitive   = true
}

variable "ssh_public_key" {
  description = "SSH public key for server access"
  type        = string
}

variable "ssh_key_name" {
  description = "Name for the SSH key in Hetzner Cloud"
  type        = string
  default     = "yolab-deployment-key"
}

variable "repo_url" {
  description = "Full repository URL for cloning"
  type        = string
}

variable "hetzner_location" {
  description = "Hetzner datacenter location"
  type        = string
  default     = "nbg1"
}

variable "server_type" {
  description = "Server type for YoLab all-in-one server"
  type        = string
  default     = "cpx11"
}

variable "environment" {
  description = "Environment name"
  type        = string
  default     = "production"
}

variable "domain" {
  description = "Domain name for the service"
  type        = string
}

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
