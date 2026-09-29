variable "do_token" {
  type      = string
  default   = ""
  sensitive = true
}

variable "spaces_access_id" {
  type      = string
  default   = ""
  sensitive = true
}

variable "spaces_secret_key" {
  type      = string
  default   = ""
  sensitive = true
}

variable "bucket_name" {
  description = <<-EOT
    Space name. These share one namespace per region, so a generic name will
    collide with someone else's and fail with a bare 409. Put something
    specific in it.
  EOT
  type        = string

  validation {
    condition     = can(regex("^[a-z0-9][a-z0-9.-]{1,61}[a-z0-9]$", var.bucket_name))
    error_message = "Space names are lowercase letters, digits, dots and hyphens, 3-63 characters, not starting or ending with punctuation."
  }
}

variable "region" {
  description = "Region for the Space. Put it in the same region as the benchmark droplet: same-region reads are the fast path, and a cross-region Space would have you benchmarking the distance between datacentres."
  type        = string
  default     = "nyc3"
}

variable "cog_path" {
  description = <<-EOT
    Local path to the generated COG to publish. Build it with:

      python3 bench/make_cog.py --out bench/data/dem.tif --size 4096 --seed 42
  EOT
  type        = string
  default     = "../../data/dem.tif"
}

variable "expected_sha256" {
  description = <<-EOT
    Optional. If set, the plan fails unless the local file hashes to this,
    so a locally regenerated COG cannot be published under the same name as
    the one your existing results were measured against.

    Leave empty on first apply, then copy the `sha256` output into here.
  EOT
  type        = string
  default     = ""
}

variable "scenes_dir" {
  description = <<-EOT
    Local directory of Sentinel-2 scenes to publish, alongside the manifest
    naming them. Populate it with:

      python3 bench/fetch_scenes.py

    Empty or missing means no scenes are published, which is a valid state:
    the synthetic DEM alone is enough to run the suite.
  EOT
  type        = string
  default     = "../../data/scenes"
}

variable "blocks_dir" {
  description = <<-EOT
    Local directory of block sets. Build it with:

      python3 bench/make_blocks.py fields.geojson

    These are published private; see scenes.tf for why.
  EOT
  type        = string
  default     = "../../blocks"
}
