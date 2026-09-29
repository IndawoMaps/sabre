# The load generator, on its own droplet.
#
# Everything here is created only when split_load_generator is true, so the
# single-box setup stays exactly as it was and costs exactly what it did.

locals {
  split = var.split_load_generator ? 1 : 0

  # Private address of the generator, or null when there isn't one. The server
  # firewall and the docker bind address both key off this.
  client_private_ip = var.split_load_generator ? digitalocean_droplet.client[0].ipv4_address_private : null

  # Empty client_size means "same as the server".
  client_size_slug = var.client_size != "" ? var.client_size : var.size

  client_size = try(data.digitalocean_sizes.client.sizes[0], null)

  # Regions offering *both* sizes. A region that has only one of them cannot
  # host a split run, and being told which regions work beats being told twice
  # that the one you picked does not.
  regions_for_pair = sort(setintersection(
    toset(try(local.size.regions, [])),
    toset(try(local.client_size.regions, [])),
  ))
}

# The region's existing default VPC, so the two boxes talk over private
# addresses. Looked up rather than created: DigitalOcean refuses to delete a
# default VPC, and a VPC created in a region that had none becomes that
# region's default -- which turns `terraform destroy` into a permanent error
# on a stack that is supposed to be disposable.
#
# Sharing the default network with whatever else is in the region is not the
# isolation boundary anyway. The firewall is: the benchmark ports open to the
# generator's private /32 and to nothing else.
data "digitalocean_vpc" "bench" {
  region = var.region
}

# The orchestrator runs on the server box and invokes bench-client on this one
# over ssh, so the server needs a key the client trusts. It is generated per
# apply and dies with the pair.
#
# The private key is stored in Terraform state. That is the standard cost of
# this pattern and it is acceptable here because the key reaches exactly one
# throwaway droplet and is replaced on the next apply -- but it does mean the
# state file is a secret. Keep it local or in an encrypted backend.
resource "tls_private_key" "orchestrator" {
  count     = local.split
  algorithm = "ED25519"
}

resource "digitalocean_droplet" "client" {
  count = local.split

  name     = "${var.name}-client"
  region   = var.region
  size     = local.client_size_slug
  image    = var.image
  ssh_keys = [for k in data.digitalocean_ssh_key.keys : k.fingerprint]
  tags     = concat(var.tags, ["load-generator"])

  vpc_uuid   = data.digitalocean_vpc.bench.id
  monitoring = true
  backups    = false

  user_data = templatefile("${path.module}/cloud-init-client.yaml", {
    orchestrator_public_key = trimspace(tls_private_key.orchestrator[0].public_key_openssh)
  })

  lifecycle {
    precondition {
      condition     = !var.split_load_generator || try(local.client_size.vcpus, 0) >= 2
      error_message = "Load-generator size '${local.client_size_slug}' has ${try(local.client_size.vcpus, 0)} vCPU. One core is not enough to offer load and measure it at the same time."
    }

    precondition {
      condition     = !var.split_load_generator || contains(try(local.client_size.regions, []), var.region)
      error_message = "Load-generator size '${local.client_size_slug}' is not offered in region '${var.region}'. It is available in: ${join(", ", try(local.client_size.regions, []))}. Regions offering both this and the server size '${var.size}': ${join(", ", local.regions_for_pair)}."
    }
  }
}

data "digitalocean_sizes" "client" {
  filter {
    key    = "slug"
    values = [local.client_size_slug]
  }
}

# Only ssh reaches the generator. It offers no service of its own.
resource "digitalocean_firewall" "client" {
  count       = local.split
  name        = "${var.name}-client-fw"
  droplet_ids = [digitalocean_droplet.client[0].id]

  inbound_rule {
    protocol         = "tcp"
    port_range       = "22"
    source_addresses = var.ssh_allowed_cidrs
  }

  # The orchestrator on the server box drives bench-client over this.
  inbound_rule {
    protocol         = "tcp"
    port_range       = "22"
    source_addresses = ["${digitalocean_droplet.bench.ipv4_address_private}/32"]
  }

  inbound_rule {
    protocol         = "icmp"
    source_addresses = concat(var.ssh_allowed_cidrs, ["${digitalocean_droplet.bench.ipv4_address_private}/32"])
  }

  outbound_rule {
    protocol              = "tcp"
    port_range            = "1-65535"
    destination_addresses = ["0.0.0.0/0", "::/0"]
  }

  outbound_rule {
    protocol              = "udp"
    port_range            = "1-65535"
    destination_addresses = ["0.0.0.0/0", "::/0"]
  }

  outbound_rule {
    protocol              = "icmp"
    destination_addresses = ["0.0.0.0/0", "::/0"]
  }
}
