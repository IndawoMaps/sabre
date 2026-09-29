terraform {
  required_version = ">= 1.6"

  required_providers {
    digitalocean = {
      source  = "digitalocean/digitalocean"
      version = "~> 2.43"
    }
    tls = {
      source  = "hashicorp/tls"
      version = "~> 4.0"
    }
  }
}

provider "digitalocean" {
  # Reads DIGITALOCEAN_TOKEN (or DIGITALOCEAN_ACCESS_TOKEN) from the environment.
  # Do not put the token in a .tf file.
  token = var.do_token != "" ? var.do_token : null
}
