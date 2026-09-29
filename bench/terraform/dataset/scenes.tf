# Real Sentinel-2 imagery and the farm blocks that sit on it.
#
# The synthetic DEM next door is a control: one float32 band, WGS84, no
# predictor, written by the same GDAL every time. These are the opposite --
# whatever AWS actually serves -- and between them they bracket the workload.
#
#   python3 bench/make_blocks.py fields.geojson
#   python3 bench/fetch_scenes.py
#   terraform apply
#
# Scenes are copied byte-exact from the public sentinel-cogs bucket, so the
# sha256 recorded here is the sha256 of the file AWS serves. A run against this
# Space is a run against production bytes, not against a re-encode of them.

locals {
  block_files = try(fileset(var.blocks_dir, "*.json"), [])

  scene_manifest_path = "${var.scenes_dir}/manifest.json"
  scene_manifest      = try(jsondecode(file(local.scene_manifest_path)), { scenes = {} })

  # file -> the scene entry that owns it, so each object can be tagged with the
  # STAC id and date it came from rather than just a filename.
  scene_of = merge([
    for set_name, s in try(local.scene_manifest.scenes, {}) : {
      for key, a in try(s.assets, {}) : a.file => {
        scene_id  = s.scene_id
        datetime  = s.datetime
        epsg      = try(s.epsg, null)
        asset     = key
        block_set = set_name
        sha256    = try(a.sha256, "")
      }
    }
  ]...)
}

# The imagery itself is published by bench/publish_scenes.py, not from here.
#
# `digitalocean_spaces_bucket_object` cannot be imported, so an object uploaded
# any other way can never be reconciled into this state -- and on a 325 MB file
# "any other way" is sometimes the only way that works. The resource makes one
# attempt with no resume, so a 1.4 GB apply that dies two thirds through leaves
# nothing to carry forward and nothing importable behind it.
#
# What stays here is what Terraform is good at: the Space itself, its lifecycle
# rules, and the small objects. The manifest below names every scene and its
# sha256, so this module still records exactly which imagery a run measured
# even though it did not upload it.

# The manifest travels with them, so anyone holding the Space can see which
# scene each file is and check it against AWS themselves.
resource "digitalocean_spaces_bucket_object" "scene_manifest" {
  count = fileexists(local.scene_manifest_path) ? 1 : 0

  region = digitalocean_spaces_bucket.bench.region
  bucket = digitalocean_spaces_bucket.bench.name
  key    = "scenes/manifest.json"
  source = local.scene_manifest_path

  acl           = "public-read"
  content_type  = "application/json"
  cache_control = "public, max-age=300"
  etag          = filemd5(local.scene_manifest_path)
}

# Private, unlike everything else here. Field boundaries say where someone's
# land is, and nothing needs these to be public: only suite.py reads them, and it gets them from the working tree that
# remote-bench.sh rsyncs up. The copy here is the archive, not the delivery
# route -- which is also why no droplet needs Spaces credentials.
resource "digitalocean_spaces_bucket_object" "blocks" {
  for_each = local.block_files

  region = digitalocean_spaces_bucket.bench.region
  bucket = digitalocean_spaces_bucket.bench.name
  key    = "blocks/${each.value}"
  source = "${var.blocks_dir}/${each.value}"

  acl           = "private"
  content_type  = "application/geo+json"
  cache_control = "no-cache"
  etag          = filemd5("${var.blocks_dir}/${each.value}")

  metadata = {
    "x-amz-meta-sha256" = filesha256("${var.blocks_dir}/${each.value}")
  }
}
