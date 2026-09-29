# The benchmark dataset, kept in a Space so it outlives any one benchmark box.
#
# This is deliberately a separate root module from ../box. `terraform destroy`
# in ../box tears down the droplet and stops the billing; the dataset survives,
# because the point of publishing it is that every run measures the same bytes.
#
# Why not just regenerate it per box? make_cog.py is seeded and deterministic in
# its *pixels*, but the file is DEFLATE-compressed by whatever libtiff/GDAL the
# image happens to ship. A different titiler image can produce a different byte
# layout -- different block sizes on disk, different range requests -- from the
# identical array. Pinning the artifact removes that variable, and the sha256
# output lets a run assert it read the same one.

locals {
  cog_sha256 = filesha256(var.cog_path)

  # The same object under both prefixes. Costs one extra copy of a 70 MB file
  # and keeps run.py's `--origin <base>` working unchanged when pointed
  # straight at the Space: it builds sources as <origin>/<server>/<file>, and
  # the split prefixes are what let per-server traffic be told apart by path.
  keys = {
    sabre   = "sabre/dem.tif"
    titiler = "titiler/dem.tif"
  }
}

resource "digitalocean_spaces_bucket" "bench" {
  name   = var.bucket_name
  region = var.region

  # The contents are a synthetic DEM that can be regenerated from a seed, so
  # there is nothing here worth protecting from a destroy.
  force_destroy = true

  versioning {
    enabled = false # every version would be another 70 MB of a file we can regenerate
  }

  lifecycle_rule {
    id                                     = "abort-incomplete-uploads"
    enabled                                = true
    abort_incomplete_multipart_upload_days = 1
  }
}

resource "digitalocean_spaces_bucket_object" "cog" {
  for_each = local.keys

  region = digitalocean_spaces_bucket.bench.region
  bucket = digitalocean_spaces_bucket.bench.name
  key    = each.value
  source = var.cog_path

  # Both servers fetch over plain anonymous HTTPS range requests. sabre speaks
  # no S3 auth, and presigned URLs expire mid-benchmark, so public-read is what
  # keeps the two servers on an identical code path. The object is a synthetic
  # DEM of nowhere.
  acl          = "public-read"
  content_type = "image/tiff"

  # Immutable by convention: republishing changed pixels means a new bucket or
  # a new key, never a silent swap under a cached URL.
  cache_control = "public, max-age=31536000, immutable"

  # Triggers a re-upload when the local file changes.
  etag = filemd5(var.cog_path)

  metadata = {
    "x-amz-meta-sha256" = local.cog_sha256
    "x-amz-meta-source" = "bench/make_cog.py --size 4096 --seed 42"
  }

  lifecycle {
    precondition {
      condition     = var.expected_sha256 == "" || var.expected_sha256 == local.cog_sha256
      error_message = "${var.cog_path} hashes to ${local.cog_sha256}, but expected_sha256 says ${var.expected_sha256}. Publishing it would silently change what every future benchmark measures. Either restore the original file or clear expected_sha256 deliberately."
    }
  }
}
