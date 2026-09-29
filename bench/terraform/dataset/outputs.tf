output "origin_base" {
  description = "Pass to run.py as --origin to read the COG straight from the Space instead of the local nginx."
  value       = "https://${digitalocean_spaces_bucket.bench.bucket_domain_name}"
}

output "download_url" {
  description = "Single URL for fetching the dataset onto a benchmark box."
  value       = "https://${digitalocean_spaces_bucket.bench.bucket_domain_name}/${local.keys.sabre}"
}

output "sha256" {
  description = "Fingerprint of the published dataset. Quote it next to benchmark results, and feed it back as expected_sha256."
  value       = local.cog_sha256
}

output "verify" {
  description = "Confirm the Space serves range requests, which is the only thing the servers need from it."
  value       = "curl -sI -r 0-1023 https://${digitalocean_spaces_bucket.bench.bucket_domain_name}/${local.keys.sabre} | head -5"
}

output "scenes" {
  description = "Sentinel-2 imagery the manifest names, as file -> the STAC id it came from. Upload it with bench/publish_scenes.py; this module does not, and scenes.tf says why."
  value       = { for f, s in local.scene_of : f => s.scene_id }
}

output "scenes_base" {
  description = "Prefix the benchmark box pulls imagery from. nginx on the box serves the same directory under /sabre/ and /titiler/, so one copy here is enough."
  value       = "https://${digitalocean_spaces_bucket.bench.bucket_domain_name}/scenes"
}

output "blocks" {
  description = "Block sets published, as file -> sha256. Private objects: suite.py reads them from the working tree rsynced onto the box, not from here."
  value       = { for f in local.block_files : f => filesha256("${var.blocks_dir}/${f}") }
}

output "publish_scenes" {
  description = "Upload the imagery the manifest names. Idempotent: an object already in the Space with the right size and sha256 is skipped, so a failed run is resumed by repeating it."
  value = join(" ", [
    "python3 bench/publish_scenes.py",
    "--bucket ${digitalocean_spaces_bucket.bench.name}",
    "--region ${digitalocean_spaces_bucket.bench.region}",
  ])
}

output "verify_scene" {
  description = "Confirm the Space serves range requests over the imagery, which is all either server needs from it."
  value = length(local.scene_of) == 0 ? "no scenes in the manifest — run: python3 bench/fetch_scenes.py" : join(" ", [
    "curl -sI -r 0-1023",
    "https://${digitalocean_spaces_bucket.bench.bucket_domain_name}/scenes/${keys(local.scene_of)[0]}",
    "| head -5",
  ])
}
