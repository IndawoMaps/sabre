#!/usr/bin/env bash
#
# Provision a split benchmark stack on DigitalOcean and run the suite on it.
#
#   bench/remote-bench.sh                       # provision, run, leave it up
#   bench/remote-bench.sh --destroy             # ... and tear it down after
#   bench/remote-bench.sh -- --duration 30      # pass args through to suite.py
#
# Two droplets: the servers and the origin on one, the load generator on the
# other, so the generator stops competing with the thing it is measuring for
# cores. The orchestrator stays on the server box because it needs docker, the
# compose file and the nginx access log; only the plan and its result cross to
# the generator.
#
# Droplets bill by the hour from the moment `terraform apply` returns. This
# script never destroys anything it did not create in this run, and never
# destroys on failure -- a broken run is exactly when you want the box alive.
# It does print the destroy command at every exit.

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/.." && pwd)"
TF_DIR="$HERE/terraform/box"

DESTROY=0
PUBLISH_BUCKET=""
# Images are pulled rather than built on the droplets. Defaults to the commit
# checked out here, so a run measures the code you are looking at; override
# IMAGE_TAG to pin something else.
IMAGE_TAG="${IMAGE_TAG:-sha-$(git -C "$(dirname "${BASH_SOURCE[0]}")/.." rev-parse HEAD 2>/dev/null | cut -c1-7)}"
[ "$IMAGE_TAG" = "sha-" ] && IMAGE_TAG=main
REGISTRY_OWNER="${REGISTRY_OWNER:-indawomaps}"
SUITE_ARGS=(--knee)
OUT_DIR="$REPO/bench/out/suite"
VERIFY=0

usage() {
    sed -n '3,/^[^#]/p' "${BASH_SOURCE[0]}" | sed '$d; s/^# \{0,1\}//'
    cat <<'EOF'

OPTIONS
    --destroy             terraform destroy after a successful run
    --publish BUCKET      publish results to this Space afterwards
    --out DIR             where to put results locally (default bench/out/suite)
    --verify              run the correctness check against titiler instead of the
                          load suite. A separate job on purpose: it answers "are the
                          numbers right", not "how fast", and it is the only thing
                          here that can catch sabre being consistently wrong.
    -h, --help            this
    --                    everything after is passed to suite.py, replacing
                          the default of --knee
EOF
}

while [ $# -gt 0 ]; do
    case "$1" in
        --destroy) DESTROY=1; shift ;;
        --publish) PUBLISH_BUCKET="${2:?--publish needs a bucket name}"; shift 2 ;;
        --out)     OUT_DIR="${2:?--out needs a directory}"; shift 2 ;;
        --verify)  VERIFY=1; shift ;;
        -h|--help) usage; exit 0 ;;
        --)        shift; SUITE_ARGS=("$@"); break ;;
        *)         echo "unknown argument: $1" >&2; usage >&2; exit 2 ;;
    esac
done

step() { printf '\n\033[1m==> %s\033[0m\n' "$*"; }
note() { printf '    %s\n' "$*"; }
die()  { printf '\n\033[31merror: %s\033[0m\n' "$*" >&2; exit 1; }

tf() { terraform -chdir="$TF_DIR" "$@"; }
tfout() { tf output -raw "$1" 2>/dev/null; }

# Run an output that is itself a shell command. An empty output would make
# `eval ""` succeed and skip the step without a word, which is how a missing
# dataset check would read as a passing one.
tfrun() {
    local cmd
    cmd="$(tfout "$1")"
    [ -n "$cmd" ] || die "terraform output '$1' is empty; the apply did not produce what this script expects"
    eval "$cmd"
}

# ── Preflight ────────────────────────────────────────────────────────────────
# Everything that can be checked without spending money, checked before
# spending any.
step "Preflight"
for c in ssh rsync ssh-keyscan ssh-keygen python3; do
    command -v "$c" >/dev/null || die "$c is not on PATH"
done
# terraform is declared in mise.toml, so it is usually present but not always
# on PATH -- mise only exports it in an activated shell.
if ! command -v terraform >/dev/null; then
    if command -v mise >/dev/null; then
        die "terraform is not on PATH. It is declared in mise.toml, so either activate mise in this shell or run:
    mise exec terraform -- $0 $*"
    fi
    die "terraform is not on PATH"
fi
[ -n "${DIGITALOCEAN_TOKEN:-}" ] || die "export DIGITALOCEAN_TOKEN first"
# The images are private, so the droplets need a registry credential. It is
# passed in at runtime and never written to Terraform state.
[ -n "${GHCR_TOKEN:-}" ] || die "export GHCR_TOKEN — the images are private.
    It must be a *classic* personal access token with the read:packages scope.
    ghcr.io does not accept fine-grained tokens, and read:packages does not
    appear in the fine-grained UI at all:
      Settings -> Developer settings -> Personal access tokens -> Tokens (classic)"
[ -n "${GHCR_USER:-}" ] || die "export GHCR_USER (your GitHub username)"
[ -f "$TF_DIR/terraform.tfvars" ] || die "no $TF_DIR/terraform.tfvars — copy example.tfvars and fill it in"
if [ -n "$PUBLISH_BUCKET" ]; then
    [ -n "${SPACES_ACCESS_KEY_ID:-}" ] && [ -n "${SPACES_SECRET_ACCESS_KEY:-}" ] \
        || die "--publish needs SPACES_ACCESS_KEY_ID and SPACES_SECRET_ACCESS_KEY (Spaces keys, not the API token)"
fi
# Fail now rather than after twenty minutes of benchmarking.
python3 -c "import boto3" 2>/dev/null || [ -z "$PUBLISH_BUCKET" ] \
    || die "--publish needs boto3: pip install boto3"
note "ok"

# ── Provision ────────────────────────────────────────────────────────────────
step "Provisioning (terraform apply)"
tf init -input=false >/dev/null
tf apply -input=false -auto-approve -var split_load_generator=true

SERVER_IP="$(tfout ip)"
CLIENT_IP="$(tfout client_ip)"
SERVER_PRIVATE="$(tfout bench_bind | cut -d= -f2)"
[ -n "$SERVER_IP" ] && [ -n "$CLIENT_IP" ] && [ -n "$SERVER_PRIVATE" ] \
    || die "terraform did not report both droplets; check the apply output"

# From here on the meter is running, so every exit path says how to stop it.
trap 'printf "\n\033[33mDroplets are still up and billing. Destroy with:\033[0m\n    terraform -chdir=%s destroy -var split_load_generator=true\n" "$TF_DIR"' EXIT

note "servers   $SERVER_IP  (private $SERVER_PRIVATE)"
note "generator $CLIENT_IP"
note "$(tfout topology)"
note "$(tfout cost)"

# ── Host keys ────────────────────────────────────────────────────────────────
# Trust on first use. DigitalOcean does not publish droplet host keys, so there
# is nothing to verify against; scanning is what `StrictHostKeyChecking=accept-new`
# would do anyway, done up front so nothing later blocks on a prompt.
#
# The -R first matters: DigitalOcean recycles addresses, and a stale entry for
# a reused IP is a hard failure rather than a prompt.
step "Trusting host keys (first use)"
mkdir -p ~/.ssh && touch ~/.ssh/known_hosts
for ip in "$SERVER_IP" "$CLIENT_IP"; do
    ssh-keygen -R "$ip" >/dev/null 2>&1 || true
    for _ in $(seq 1 60); do
        if ssh-keyscan -T 5 "$ip" 2>/dev/null | grep -q .; then
            ssh-keyscan -T 5 "$ip" 2>/dev/null >> ~/.ssh/known_hosts
            break
        fi
        sleep 5
    done
done
note "done"

ssh_to() { ssh -o BatchMode=yes -o ConnectTimeout=15 "root@$1" "${@:2}"; }

# ── Wait for cloud-init ──────────────────────────────────────────────────────
step "Waiting for cloud-init on both droplets"
for ip in "$SERVER_IP" "$CLIENT_IP"; do
    for _ in $(seq 1 60); do
        ssh_to "$ip" true 2>/dev/null && break
        sleep 5
    done
    ssh_to "$ip" 'cloud-init status --wait >/dev/null' \
        || die "cloud-init did not finish cleanly on $ip; ssh in and read /var/log/cloud-init-output.log"
    note "$ip ready"
done

# ── Dataset ──────────────────────────────────────────────────────────────────
# cloud-init downloads and verifies it. This re-hashes rather than trusting the
# marker, because the marker records that a download once succeeded, not that
# the file is still there.
step "Verifying the dataset on the server"
if ! tfrun check_dataset; then
    note "missing or wrong — refetching"
    tfrun refetch_dataset
    tfrun check_dataset || die "dataset still wrong after refetch"
fi

# Imagery, if any is published. cloud-init fails the boot on a bad fetch, so
# reaching here with wrong scenes means something removed or altered them since.
tfrun check_scenes || die "the Sentinel-2 scenes on the box do not match the manifest.
    Nothing here can repair that safely -- the box would benchmark different bytes
    than the results claim. Destroy it and apply again:
      cd bench/terraform/box && terraform destroy && terraform apply"

# ── Source ───────────────────────────────────────────────────────────────────
step "Pushing source to both droplets"
tfrun push
tfrun push_client
note "done"

# The push excludes bench/data, so the dataset survives. Confirm rather than
# assume: this is the step that has gone wrong before.
tfrun check_dataset >/dev/null || die "the push removed the dataset; do not trust results from this box"
tfrun check_scenes >/dev/null || die "the push removed the Sentinel-2 scenes; do not trust results from this box"

# ── Images ───────────────────────────────────────────────────────────────────
step "Logging both droplets into ghcr.io"
for ip in "$SERVER_IP" "$CLIENT_IP"; do
    # --password-stdin keeps the token out of the process list on the droplet.
    printf '%s' "$GHCR_TOKEN" | ssh_to "$ip" "docker login ghcr.io -u '$GHCR_USER' --password-stdin" >/dev/null \
        || die "docker login failed on $ip — check GHCR_TOKEN has read:packages"
done
note "ok"

step "Installing bench-client on the generator (pull, not build)"
ssh_to "$CLIENT_IP" "cd /root/sabre && BENCH_CLIENT_IMAGE=ghcr.io/$REGISTRY_OWNER/sabre-bench-client:$IMAGE_TAG bash bench/install-client.sh" \
    || die "could not install bench-client at tag $IMAGE_TAG. Has the release workflow published it? Try IMAGE_TAG=main, or append --build to compile from source." 

# ── Servers ──────────────────────────────────────────────────────────────────
# BENCH_BIND publishes on the private interface, so the benchmark ports never
# appear on the public one and the generator can still reach them.
step "Starting the servers"
note "sabre image: ghcr.io/$REGISTRY_OWNER/sabre-server:$IMAGE_TAG"
# No --build: the image comes from the registry, so the box never compiles.
ssh_to "$SERVER_IP" "cd /root/sabre \
    && export BENCH_BIND=$SERVER_PRIVATE SABRE_IMAGE=ghcr.io/$REGISTRY_OWNER/sabre-server:$IMAGE_TAG \
    && docker compose -f bench/docker-compose.yml pull -q \
    && docker compose -f bench/docker-compose.yml up -d" 

note "waiting for both to answer on the private interface"
ssh_to "$SERVER_IP" "
    for _ in \$(seq 1 60); do
        curl -sf http://$SERVER_PRIVATE:8787/health >/dev/null \
            && curl -sf http://$SERVER_PRIVATE:8000/healthz >/dev/null && exit 0
        sleep 5
    done
    echo 'servers did not come up on $SERVER_PRIVATE' >&2; exit 1
" || die "servers never became healthy; ssh in and check 'docker compose -f bench/docker-compose.yml logs'"

# ── Run ──────────────────────────────────────────────────────────────────────
if [ "$VERIFY" -eq 1 ]; then
    step "Checking sabre against titiler"
    note "correctness, not speed — see bench/verify.py"
    # Failure is recorded rather than fatal, so the box is still torn down and
    # the report still comes back. It decides the exit code at the very end.
    ssh_to "$SERVER_IP" "cd /root/sabre && python3 bench/verify.py \
        --sabre http://$SERVER_PRIVATE:8787 \
        --titiler http://$SERVER_PRIVATE:8000 \
        --out bench/out/verify \
        $(printf '%q ' "${SUITE_ARGS[@]}")" || VERIFY_FAILED=1
    REMOTE_RESULTS=bench/out/verify
else
    step "Running the suite"
    note "generator: $CLIENT_IP   servers: $SERVER_PRIVATE"

    # The box gets the working tree by rsync with .git excluded, so git cannot
    # tell it what it is running. Capture that here, where the repository is,
    # and hand it over. base64 so a subject line with quotes in it cannot break
    # the ssh command.
    GIT_B64="$(python3 "$HERE/suite.py" --print-git | base64 | tr -d '\n')" \
        || die "could not read this checkout's git metadata; run from inside the repository"

    ssh_to "$SERVER_IP" "cd /root/sabre && SABRE_BENCH_GIT_B64=$GIT_B64 python3 bench/suite.py \
        --client-ssh bench-client \
        --client /root/sabre/target/release/bench-client \
        --sabre http://$SERVER_PRIVATE:8787 \
        --out bench/out/suite \
        $(printf '%q ' "${SUITE_ARGS[@]}")"
    REMOTE_RESULTS=bench/out/suite
fi

# ── Results ──────────────────────────────────────────────────────────────────
step "Fetching results"
mkdir -p "$OUT_DIR"
rsync -az --delete "root@$SERVER_IP:/root/sabre/$REMOTE_RESULTS/" "$OUT_DIR/"
note "$OUT_DIR"

if [ "$VERIFY" -eq 0 ] && [ -n "$PUBLISH_BUCKET" ]; then
    step "Publishing"
    python3 "$HERE/publish.py" --bucket "$PUBLISH_BUCKET" --result "$OUT_DIR/suite.json"
    python3 "$HERE/publish.py" --bucket "$PUBLISH_BUCKET" --compare main || true
fi

# ── Teardown ─────────────────────────────────────────────────────────────────
if [ -n "${VERIFY_FAILED:-}" ] && [ "$DESTROY" -eq 0 ]; then
    note "correctness checks failed — see $OUT_DIR/verify.md"
fi

if [ "$DESTROY" -eq 1 ]; then
    step "Destroying"
    trap - EXIT
    tf destroy -input=false -auto-approve -var split_load_generator=true
    printf '\n\033[32mDone. Results in %s. Nothing left running.\033[0m\n' "$OUT_DIR"
else
    step "Done"
    note "Results in $OUT_DIR"
fi

# Decided last, so a failed check still tears the box down and still brings the
# report back. Only the exit code carries the verdict.
if [ -n "${VERIFY_FAILED:-}" ]; then
    printf '\n\033[31mCorrectness checks failed. See %s/verify.md\033[0m\n' "$OUT_DIR" >&2
    exit 1
fi
