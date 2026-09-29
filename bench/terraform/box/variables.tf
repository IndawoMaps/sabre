variable "do_token" {
  description = "DigitalOcean API token. Prefer exporting DIGITALOCEAN_TOKEN instead of setting this."
  type        = string
  default     = ""
  sensitive   = true
}

variable "name" {
  description = "Name for the droplet and its firewall."
  type        = string
  default     = "sabre-bench"
}

variable "region" {
  description = "DigitalOcean region slug. Pick one near you; latency to the box affects nothing in the benchmark (the load generator runs on it) but affects your ssh comfort."
  type        = string
  default     = "nyc3"
}

variable "size" {
  description = <<-EOT
    Droplet size slug. The default is CPU-Optimized Premium Intel: 4 dedicated
    Intel vCPUs, 8 GB RAM, NVMe SSD.

    Dedicated matters. Shared-CPU sizes (s-*) are burstable and their steal time
    makes benchmark numbers irreproducible, which defeats the point of the box.

    `terraform plan` fails if this slug does not have 4 vCPUs and 8 GB, so a
    typo or a renamed slug is caught before anything is billed.
  EOT
  type        = string
  default     = "c-4-intel"
}

variable "image" {
  description = "Base image slug."
  type        = string
  default     = "ubuntu-24-04-x64"
}

variable "ssh_key_names" {
  description = <<-EOT
    Names of SSH keys already in your DigitalOcean account, as shown by
    `doctl compute ssh-key list`. At least one is required: the droplet is
    created with password authentication disabled, so without a key you
    cannot log in.
  EOT
  type        = list(string)

  validation {
    condition     = length(var.ssh_key_names) > 0
    error_message = "Provide at least one SSH key name, or you will not be able to reach the droplet."
  }
}

variable "ssh_allowed_cidrs" {
  description = <<-EOT
    CIDRs allowed to reach port 22. Defaults to the whole internet, which is
    survivable because the droplet is key-only, but narrowing it to your own
    address is free:

      ssh_allowed_cidrs = ["$(curl -s ifconfig.me)/32"]
  EOT
  type        = list(string)
  default     = ["0.0.0.0/0"]
}

variable "expose_bench_ports" {
  description = <<-EOT
    Open 8000/8787/8081 (titiler, sabre, origin) to `ssh_allowed_cidrs`.

    Off by default: run.py drives both servers over 127.0.0.1 from the droplet
    itself, so nothing needs to be reachable from outside. Turn it on only to
    poke at a server by hand, and note that driving load across the internet
    measures your home uplink, not the server.
  EOT
  type        = bool
  default     = false
}

variable "tags" {
  description = "Tags applied to the droplet."
  type        = list(string)
  default     = ["sabre", "bench", "ephemeral"]
}

variable "dataset_bucket" {
  description = <<-EOT
    Name of the Space holding the benchmark COG, created by ../dataset.
    The droplet downloads the dataset from it on first boot instead of
    regenerating it, so every box measures the identical bytes.
  EOT
  type        = string
}

variable "dataset_sha256" {
  description = <<-EOT
    The `sha256` output of ../dataset. cloud-init verifies the download
    against it and refuses to leave a mismatched file in place, so a run
    cannot quietly measure a different dataset than the one you think.
  EOT
  type        = string

  validation {
    condition     = can(regex("^[a-f0-9]{64}$", var.dataset_sha256))
    error_message = "Expected a 64-character hex sha256, as printed by `terraform -chdir=../dataset output -raw sha256`."
  }
}

variable "split_load_generator" {
  description = <<-EOT
    Run the load generator on a second droplet instead of on the server itself.

    On one box the generator competes with the thing it is measuring for the
    same cores, which caps the fastest suites: sabre's ~4,000 req/s `info`
    figure is a floor set by the client, not a ceiling set by the server.

    It is not free. The two boxes talk over the VPC, which adds a few tenths of
    a millisecond to every request. That is noise against a 26 ms polygon and
    it is the entire measurement for a 0.3 ms `/info`, so suite.py keeps the
    latency-sensitive suites on loopback and measures the floor either way.
  EOT
  type        = bool
  default     = false
}

variable "client_size" {
  description = <<-EOT
    Size of the load-generator droplet. Empty means "the same as the server",
    which is the default and what you want unless you have a reason otherwise.

    Matching the server is not about needing four cores to generate load --
    bench-client needs roughly one to offer 4,000 req/s. It is about the
    generator never being the suspect. A smaller box is cheaper right up to
    the first surprising result, at which point the first question is whether
    the client ran out of room, and you cannot answer it after the fact.

    Tying it to var.size also means one size to check against one region: a
    mismatched pair that exists in different regions cannot happen.
  EOT
  type        = string
  default     = ""
}


variable "scenes_dir" {
  description = <<-EOT
    Local directory holding the Sentinel-2 manifest written by
    bench/fetch_scenes.py. Terraform reads it to learn which scenes the box
    should pull from the Space; the imagery itself is never uploaded from
    here, only named.

    Missing or empty means the box fetches only the synthetic DEM, which is a
    valid configuration -- the suite skips the imagery workloads.
  EOT
  type        = string
  default     = "../../data/scenes"
}
