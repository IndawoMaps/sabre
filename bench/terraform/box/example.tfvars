# cp example.tfvars terraform.tfvars, then edit.

# `doctl compute ssh-key list` -- the name column, not the fingerprint.
ssh_key_names = ["my-laptop"]

# From ../dataset:
#   terraform -chdir=../dataset output -raw sha256
dataset_bucket = "sabre-bench-CHANGEME"
dataset_sha256 = "0000000000000000000000000000000000000000000000000000000000000000"

# Must match the Space's region, or every range request crosses datacentres.
# region = "nyc3"

# Narrow this to yourself: ["$(curl -s ifconfig.me)/32"]
# ssh_allowed_cidrs = ["0.0.0.0/0"]
