variable "hcloud_token" {
  description = "Hetzner Cloud API token"
  type        = string
  sensitive   = true
}

variable "ssh_public_key" {
  description = "SSH public key for server access"
  type        = string
}

variable "ssh_private_key" {
  description = "SSH private key for server deployment"
  type        = string
  sensitive   = true
}

variable "ssh_key_name" {
  description = "Name for the SSH key in Hetzner Cloud"
  type        = string
  default     = "yolab-deployment-key"
}

variable "hetzner_location" {
  description = "Hetzner datacenter location"
  type        = string
  default     = "nbg1"
}

variable "wg_server_type" {
  description = "Server type for WireGuard server"
  type        = string
  default     = "cpx22"
}

variable "services_server_type" {
  description = "Server type for services stack"
  type        = string
  default     = "cpx22"
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
