output "ip" {
  description = "Public IPv4 address."
  value       = digitalocean_droplet.bench.ipv4_address
}

output "hardware" {
  description = "What the benchmark actually ran on. Quote this alongside the numbers."
  value = format(
    "%s — %d vCPU, %d MB RAM, %d GB SSD, %s, %s",
    var.size,
    try(local.size.vcpus, 0),
    try(local.size.memory, 0),
    try(local.size.disk, 0),
    var.image,
    var.region,
  )
}

output "cost" {
  description = "Live rate from the DigitalOcean API, not a hardcoded guess. Droplets bill by the hour."
  value = format(
    "$%.3f/hour (caps at $%.2f/month if left running)",
    try(local.size.price_hourly, 0),
    try(local.size.price_monthly, 0),
  )
}

output "wait_until_ready" {
  description = "cloud-init installs Docker after the droplet reports active; this blocks until that finished."
  value       = "ssh root@${digitalocean_droplet.bench.ipv4_address} 'cloud-init status --wait && cat /var/lib/cloud/bench-ready'"
}

output "push" {
  description = "Copy the working tree up, excluding build output and previous results."
  value = join(" ", [
    "rsync -az --delete",
    "--exclude .git --exclude target --exclude node_modules",
    "--exclude bench/data --exclude bench/out",
    "${abspath("${path.module}/../../..")}/",
    "root@${digitalocean_droplet.bench.ipv4_address}:/root/sabre/",
  ])
}

output "pull_results" {
  description = "Fetch results back. Clear bench/out locally first so two runs cannot be read as one."
  value       = "rm -rf bench/out && rsync -az root@${digitalocean_droplet.bench.ipv4_address}:/root/sabre/bench/out/ bench/out/"
}

output "ssh" {
  value = "ssh root@${digitalocean_droplet.bench.ipv4_address}"
}

output "dataset" {
  description = "Where the COG came from and what it hashes to. Quote this next to results; two runs are only comparable if it matches."
  value       = "https://${data.digitalocean_spaces_bucket.dataset.bucket_domain_name}/sabre/dem.tif (sha256 ${var.dataset_sha256})"
}

output "check_dataset" {
  description = <<-EOT
    Re-hash the dataset on the box and compare it to dataset_sha256. Exits
    non-zero if it is missing or wrong.

    This hashes the file rather than reading the marker cloud-init wrote,
    because the marker records that the download once succeeded, not that the
    file is still there -- and an rsync with --delete is entirely capable of
    removing it afterwards.
  EOT
  value = join(" ", [
    "ssh root@${digitalocean_droplet.bench.ipv4_address}",
    "'f=/root/sabre/bench/data/dem.tif;",
    "[ -f $f ] || { echo \"MISSING: $f\"; exit 1; };",
    "got=$(sha256sum $f | cut -d\" \" -f1);",
    "[ \"$got\" = \"${var.dataset_sha256}\" ] && echo \"dataset ok: $got\"",
    "|| { echo \"MISMATCH: got $got want ${var.dataset_sha256}\"; exit 1; }'",
  ])
}

output "refetch_dataset" {
  description = "Re-download the dataset onto the box, for when check_dataset says it is missing."
  value = join(" ", [
    "ssh root@${digitalocean_droplet.bench.ipv4_address}",
    "'mkdir -p /root/sabre/bench/data &&",
    "curl -fsSL --retry 5 -o /root/sabre/bench/data/dem.tif",
    "https://${data.digitalocean_spaces_bucket.dataset.bucket_domain_name}/sabre/dem.tif'",
  ])
}

output "client_ip" {
  description = "Load generator's public address. Null unless split_load_generator is set."
  value       = var.split_load_generator ? digitalocean_droplet.client[0].ipv4_address : null
}

output "topology" {
  description = "Where load comes from. Record it: a split-generator run and a loopback run are not directly comparable."
  value = var.split_load_generator ? format(
    "split — servers on %s (%s), generator on %s (%s), over VPC %s",
    digitalocean_droplet.bench.ipv4_address_private, var.size,
    local.client_private_ip, local.client_size_slug,
    data.digitalocean_vpc.bench.ip_range,
    ) : format(
    "single box — generator shares %s with the servers; the fastest suites are client-limited",
    var.size,
  )
}

output "suite" {
  description = "Run the suite with the generator on the other droplet. Servers are addressed by private IP because that is the interface the generator can reach."
  value = var.split_load_generator ? join(" ", [
    "ssh root@${digitalocean_droplet.bench.ipv4_address}",
    "'cd /root/sabre &&",
    "python3 bench/suite.py --knee",
    "--client-ssh bench-client",
    "--client /root/sabre/target/release/bench-client",
    "--sabre http://${digitalocean_droplet.bench.ipv4_address_private}:8787",
    "--titiler http://${digitalocean_droplet.bench.ipv4_address_private}:8000",
    "--out bench/out/suite'",
  ]) : "ssh root@${digitalocean_droplet.bench.ipv4_address} 'cd /root/sabre && python3 bench/suite.py --knee --out bench/out/suite'"
}

output "push_client" {
  description = "Source for the generator, so bench-client can be built there. Null unless split."
  value = var.split_load_generator ? join(" ", [
    "rsync -az --delete --exclude .git --exclude target --exclude node_modules",
    "--exclude bench/data --exclude bench/out",
    "${abspath("${path.module}/../../..")}/",
    "root@${digitalocean_droplet.client[0].ipv4_address}:/root/sabre/",
  ]) : null
}

output "install_client" {
  description = "Put bench-client on the generator by pulling the published image. BENCH_CLIENT_IMAGE pins a sha- tag; --build compiles from source instead."
  value       = var.split_load_generator ? "ssh root@${digitalocean_droplet.client[0].ipv4_address} 'cd /root/sabre && bash bench/install-client.sh'" : null
}

output "bench_bind" {
  description = "Export this before `docker compose up` so the servers listen where the generator can reach them, and nowhere else."
  value       = var.split_load_generator ? "BENCH_BIND=${digitalocean_droplet.bench.ipv4_address_private}" : "BENCH_BIND=127.0.0.1"
}

output "regions_for_pair" {
  description = "Regions offering both the server size and the load-generator size. Only these can host a split run."
  value       = local.regions_for_pair
}

output "client_private_ip" {
  description = "Generator's private address — what the server's `bench-client` ssh alias resolves to."
  value       = var.split_load_generator ? digitalocean_droplet.client[0].ipv4_address_private : null
}

output "scenes" {
  description = "Sentinel-2 imagery this box will pull, as file -> sha256. Empty means the suite runs on the synthetic DEM alone."
  value       = local.scene_assets
}

output "check_scenes" {
  description = <<-EOT
    Re-hash the imagery on the box. Like check_dataset, this hashes the files
    rather than reading cloud-init's marker: the marker says the download
    once succeeded, not that an rsync with --delete has left them alone.
  EOT
  value = length(local.scene_assets) == 0 ? "echo 'no scenes configured'" : join(" ", concat([
    "ssh root@${digitalocean_droplet.bench.ipv4_address}",
    "'cd /root/sabre/bench/data/scenes 2>/dev/null || { echo \"MISSING: scenes directory\"; exit 1; };",
    ], [
    for file, sha in local.scene_assets :
    "[ \"$(sha256sum ${file} 2>/dev/null | cut -d\" \" -f1)\" = \"${sha}\" ] || { echo \"BAD: ${file}\"; exit 1; };"
    ], [
    "echo \"${length(local.scene_assets)} scenes ok\"'",
  ]))
}
