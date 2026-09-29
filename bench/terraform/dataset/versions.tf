terraform {
  required_version = ">= 1.6"

  required_providers {
    digitalocean = {
      source  = "digitalocean/digitalocean"
      version = "~> 2.43"
    }
  }
}

provider "digitalocean" {
  # DIGITALOCEAN_TOKEN creates the Space. Uploading objects into it needs a
  # *separate* pair of S3-style credentials, made under API -> Spaces Keys:
  #
  #   export DIGITALOCEAN_TOKEN=dop_v1_...
  #   export SPACES_ACCESS_KEY_ID=...
  #   export SPACES_SECRET_ACCESS_KEY=...
  #
  # The API token alone is not enough and the error when it is missing is not
  # obvious, so check these first if an upload fails.
  token             = var.do_token != "" ? var.do_token : null
  spaces_access_id  = var.spaces_access_id != "" ? var.spaces_access_id : null
  spaces_secret_key = var.spaces_secret_key != "" ? var.spaces_secret_key : null
}
