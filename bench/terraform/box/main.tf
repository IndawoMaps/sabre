# An ephemeral benchmark box for bench/run.py.
#
#   terraform apply     # ~2 min, billing starts
#   ... run the benchmark, pull the results ...
#   terraform destroy   # billing stops
#
# DigitalOcean bills droplets by the hour, so the cost of a benchmark run is
# the cost of the hour it runs in. `terraform output` prints the real rate,
# read from the API rather than hardcoded here.

# Created by ../dataset. Looked up rather than passed as a URL so a typo is a
# plan-time error instead of a 404 halfway through cloud-init.
data "digitalocean_spaces_bucket" "dataset" {
  name   = var.dataset_bucket
  region = var.region
}

data "digitalocean_ssh_key" "keys" {
  for_each = toset(var.ssh_key_names)
  name     = each.value
}

# Used to fail the plan on a bad size slug, and to report the real price.
data "digitalocean_sizes" "selected" {
  filter {
    key    = "slug"
    values = [var.size]
  }
}

locals {
  size = try(data.digitalocean_sizes.selected.sizes[0], null)

  # Which Sentinel-2 scenes to pull, read from the manifest fetch_scenes.py
  # wrote. Terraform only names them; the bytes come from the Space, which is
  # in the same region as the box.
  scene_manifest = try(jsondecode(file("${path.module}/${var.scenes_dir}/manifest.json")), { scenes = {} })
  scene_assets = merge([
    for set_name, s in try(local.scene_manifest.scenes, {}) : {
      for key, a in try(s.assets, {}) : a.file => try(a.sha256, "")
    }
  ]...)

  # One "<sha256> <url> <filename>" line per scene, or empty. cloud-init reads
  # it from a file rather than a heredoc: the YAML block scalar and the shell
  # both care about indentation, and a file sidesteps having to satisfy each.
  scenes_tsv = join("\n", [
    for file, sha in local.scene_assets :
    "${sha} https://${data.digitalocean_spaces_bucket.dataset.bucket_domain_name}/scenes/${file} ${file}"
  ])

  # The benchmark is only comparable against previous runs if the hardware is
  # the same shape every time, so the shape is asserted rather than assumed.
  want_vcpus  = 4
  want_memory = 8192 # MB
}

resource "digitalocean_droplet" "bench" {
  name     = var.name
  region   = var.region
  size     = var.size
  image    = var.image
  ssh_keys = [for k in data.digitalocean_ssh_key.keys : k.fingerprint]
  tags     = var.tags

  monitoring = true
  backups    = false # an ephemeral box has nothing worth backing up
  ipv6       = true

  # Same VPC as the generator, so benchmark traffic stays on private addresses.
  vpc_uuid = var.split_load_generator ? data.digitalocean_vpc.bench.id : null

  user_data = templatefile("${path.module}/cloud-init.yaml", {
    dataset_url    = "https://${data.digitalocean_spaces_bucket.dataset.bucket_domain_name}/sabre/dem.tif"
    dataset_sha256 = var.dataset_sha256
    scenes         = local.scenes_tsv

    # Lets the orchestrator on this box drive bench-client on the generator.
    # Empty in the single-box setup, where nothing is driven remotely.
    orchestrator_private_key = var.split_load_generator ? tls_private_key.orchestrator[0].private_key_openssh : ""

    # The alias needs an address to resolve to. Taking it from the client
    # resource makes this droplet depend on that one, which is fine: the
    # generator references nothing here, so the dependency runs one way.
    client_private_ip = var.split_load_generator ? digitalocean_droplet.client[0].ipv4_address_private : ""
  })

  lifecycle {
    precondition {
      condition     = local.size != null
      error_message = "Unknown droplet size '${var.size}'. List valid slugs with: doctl compute size list"
    }

    precondition {
      condition     = local.size != null && try(local.size.vcpus, 0) == local.want_vcpus && try(local.size.memory, 0) == local.want_memory
      error_message = "Size '${var.size}' is ${try(local.size.vcpus, 0)} vCPU / ${try(local.size.memory, 0)} MB, but the benchmark box is specified as ${local.want_vcpus} vCPU / ${local.want_memory} MB. Benchmark numbers are not comparable across different hardware."
    }

    precondition {
      condition     = local.size != null && contains(try(local.size.regions, []), var.region)
      error_message = "Server size '${var.size}' is not offered in region '${var.region}'. It is available in: ${join(", ", try(local.size.regions, []))}."
    }
  }
}

resource "digitalocean_firewall" "bench" {
  name        = "${var.name}-fw"
  droplet_ids = [digitalocean_droplet.bench.id]

  inbound_rule {
    protocol         = "tcp"
    port_range       = "22"
    source_addresses = var.ssh_allowed_cidrs
  }

  # Off unless expose_bench_ports is set: with the generator on this box, both
  # servers are driven over loopback and nothing needs to be reachable at all.
  dynamic "inbound_rule" {
    for_each = var.expose_bench_ports ? ["8787", "8000", "8081"] : []
    content {
      protocol         = "tcp"
      port_range       = inbound_rule.value
      source_addresses = var.ssh_allowed_cidrs
    }
  }

  # With a split generator the servers must be reachable from it -- and from
  # nothing else. A single private /32, not the VPC range: another droplet in
  # the same VPC has no business generating load against a benchmark.
  dynamic "inbound_rule" {
    for_each = var.split_load_generator ? ["8787", "8000", "8081"] : []
    content {
      protocol         = "tcp"
      port_range       = inbound_rule.value
      source_addresses = ["${local.client_private_ip}/32"]
    }
  }

  inbound_rule {
    protocol         = "icmp"
    source_addresses = var.ssh_allowed_cidrs
  }

  # Unrestricted egress: the box pulls Docker images, crates and apt packages.
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
