#!/usr/bin/env python3
"""Upload the Sentinel-2 scenes to the benchmark Space.

Terraform manages the Space, the synthetic DEM, the block sets and the scene
manifest. It does not manage the imagery, and this is why:

  * `digitalocean_spaces_bucket_object` has no import support, so an object
    uploaded any other way can never be reconciled into state. That makes the
    resource all-or-nothing on files where "any other way" is sometimes the
    only way that works.
  * A 325 MB object wants a multipart upload that can resume. The resource
    does one attempt, and a 1.4 GB apply that fails two thirds of the way in
    leaves nothing to carry forward.

So the imagery is published by this, which is idempotent: an object already in
the Space with the right size and recorded sha256 is skipped, and a run that
dies halfway is resumed by running it again.

    export SPACES_ACCESS_KEY_ID=... SPACES_SECRET_ACCESS_KEY=...
    python3 bench/publish_scenes.py --bucket sabre-bench-tf

Needs boto3, as bench/publish.py does.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import sys
import threading
import time

HERE = os.path.dirname(os.path.abspath(__file__))
SCENES = os.path.join(HERE, "data", "scenes")

try:
    import boto3
    from boto3.s3.transfer import TransferConfig
    from botocore.config import Config
    from botocore.exceptions import ClientError
except ImportError:
    boto3 = None

# Everything below mirrors what bench/terraform/dataset sets on the DEM, so the
# two kinds of object in this Space are described the same way.
CONTENT_TYPE = "image/tiff"
CACHE_CONTROL = "public, max-age=31536000, immutable"
SOURCE = "sentinel-cogs.s3.us-west-2.amazonaws.com, copied byte-exact"

# 32 MB parts: DigitalOcean allows 1000 parts per upload, which leaves plenty
# of headroom on the largest asset and keeps a failed part cheap to retry.
PART_BYTES = 32 << 20


def client(region: str, endpoint: str | None):
    if boto3 is None:
        sys.exit("boto3 is required:  pip install boto3   (or apt install python3-boto3)")
    key = os.environ.get("SPACES_ACCESS_KEY_ID") or os.environ.get("AWS_ACCESS_KEY_ID")
    secret = os.environ.get("SPACES_SECRET_ACCESS_KEY") or os.environ.get("AWS_SECRET_ACCESS_KEY")
    if not key or not secret:
        sys.exit("set SPACES_ACCESS_KEY_ID and SPACES_SECRET_ACCESS_KEY "
                 "(Spaces keys, not the DigitalOcean API token)")
    return boto3.client(
        "s3",
        region_name=region,
        endpoint_url=endpoint or f"https://{region}.digitaloceanspaces.com",
        aws_access_key_id=key,
        aws_secret_access_key=secret,
        config=Config(retries={"max_attempts": 5, "mode": "standard"}),
    )


def sha256_of(path: str) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as fh:
        for chunk in iter(lambda: fh.read(1 << 22), b""):
            h.update(chunk)
    return h.hexdigest()


def already_published(s3, bucket: str, key: str, sha: str, size: int) -> bool:
    """Whether the Space already holds exactly this object.

    Compares the recorded sha256 rather than the ETag: a multipart ETag is a
    hash of part hashes and depends on the part size, so it says nothing about
    whether two uploads produced the same bytes.
    """
    try:
        head = s3.head_object(Bucket=bucket, Key=key)
    except ClientError as exc:
        if exc.response["Error"]["Code"] in ("404", "NoSuchKey"):
            return False
        raise
    return (head["ContentLength"] == size
            and head.get("Metadata", {}).get("x-amz-meta-sha256") == sha)


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--bucket", required=True)
    ap.add_argument("--region", default="nyc3")
    ap.add_argument("--endpoint", default=None)
    ap.add_argument("--scenes-dir", default=SCENES)
    ap.add_argument("--prefix", default="scenes")
    ap.add_argument("--force", action="store_true",
                    help="re-upload even objects already present and correct")
    ap.add_argument("--dry-run", action="store_true")
    args = ap.parse_args()

    manifest_path = os.path.join(args.scenes_dir, "manifest.json")
    if not os.path.exists(manifest_path):
        sys.exit(f"no manifest at {manifest_path}. Run bench/fetch_scenes.py first.")
    with open(manifest_path) as fh:
        manifest = json.load(fh)

    targets = []
    for set_name, scene in sorted(manifest.get("scenes", {}).items()):
        for asset, a in sorted(scene.get("assets", {}).items()):
            path = os.path.join(args.scenes_dir, a["file"])
            if not os.path.exists(path):
                sys.exit(f"{a['file']} is in the manifest but not on disk. "
                         f"Run bench/fetch_scenes.py.")
            targets.append((path, a, scene, asset))
    if not targets:
        sys.exit("the manifest names no assets")

    s3 = client(args.region, args.endpoint)

    print("Checking local copies against the manifest")
    plan = []
    for path, a, scene, asset in targets:
        size = os.path.getsize(path)
        got = sha256_of(path)
        if got != a["sha256"]:
            sys.exit(f"{a['file']} hashes to {got}, but the manifest says {a['sha256']}.\n"
                     f"This is not the file AWS served; re-download it rather than "
                     f"publishing it.")
        key = f"{args.prefix}/{a['file']}"
        if not args.force and already_published(s3, args.bucket, key, got, size):
            print(f"  {a['file']:42} already published")
            continue
        plan.append((path, key, got, size, scene, asset))

    if not plan:
        print("\nEverything in the manifest is already in the Space.")
        return

    total = sum(size for _, _, _, size, _, _ in plan)
    print(f"\n{len(plan)} to upload, {total / 1e6:,.0f} MB")
    if args.dry_run:
        for _, key, _, size, _, _ in plan:
            print(f"  would upload {key}  {size / 1e6:.1f} MB")
        return

    cfg = TransferConfig(multipart_threshold=PART_BYTES, multipart_chunksize=PART_BYTES,
                         max_concurrency=4, use_threads=True)
    started = time.time()
    sent_total = 0

    for path, key, sha, size, scene, asset in plan:
        sent = [0]
        lock = threading.Lock()

        def progress(n, sent=sent, lock=lock, size=size, key=key, base=sent_total):
            with lock:
                sent[0] += n
                rate = (base + sent[0]) / max(time.time() - started, 0.1) / 1e6
                print(f"\r  {os.path.basename(key):42} {100 * sent[0] / size:5.1f}%  "
                      f"{rate:5.1f} MB/s", end="", flush=True)

        s3.upload_file(
            path, args.bucket, key,
            ExtraArgs={
                # Public-read like the DEM, and for the same reason: sabre
                # speaks no S3 auth, so anonymous range requests are the only
                # code path both servers share. These are bytes AWS already
                # publishes to the world.
                "ACL": "public-read",
                "ContentType": CONTENT_TYPE,
                "CacheControl": CACHE_CONTROL,
                "Metadata": {
                    "x-amz-meta-sha256": sha,
                    "x-amz-meta-scene-id": scene["scene_id"],
                    "x-amz-meta-datetime": scene["datetime"],
                    "x-amz-meta-asset": asset,
                    "x-amz-meta-source": SOURCE,
                },
            },
            Config=cfg, Callback=progress)
        sent_total += size
        print(f"\r  {os.path.basename(key):42} done   {size / 1e6:7.1f} MB")

    print(f"\n{total / 1e6:,.0f} MB in {time.time() - started:.0f}s")

    print("\nVerifying")
    bad = 0
    for _, key, sha, size, _, _ in plan:
        head = s3.head_object(Bucket=args.bucket, Key=key)
        ok = (head["ContentLength"] == size
              and head.get("Metadata", {}).get("x-amz-meta-sha256") == sha)
        bad += not ok
        print(f"  {'ok ' if ok else 'BAD'} {key}")
    if bad:
        sys.exit(f"{bad} object(s) did not land correctly; re-run to retry them")

    base = f"https://{args.bucket}.{args.region}.digitaloceanspaces.com/{args.prefix}"
    print(f"\n{base}/")
    print("Confirm the Space serves range requests, which is all either server needs:")
    print(f"  curl -sI -r 0-1023 {base}/{os.path.basename(plan[0][1])} | head -5")


if __name__ == "__main__":
    main()
