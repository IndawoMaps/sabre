#!/usr/bin/env python3
"""Publish benchmark results to a Space, and build an index over them.

    python3 bench/publish.py --bucket sabre-bench-you --result bench/out/suite/suite.json
    python3 bench/publish.py --bucket sabre-bench-you --index-only
    python3 bench/publish.py --bucket sabre-bench-you --compare main

Layout:

    results/<branch>/<commit>/<timestamp>.json     one run, immutable
    index.json                                     every run, summarised

Results are immutable and the index is *rebuilt by listing them*, never
appended to. Two machines publishing at once would otherwise read the same
index, each add their own row, and the second write would silently drop the
first. Rebuilding from a listing makes the objects the source of truth and the
index a derived cache, so the worst a concurrent write costs is a stale index
that the next publish repairs.

Needs boto3 and Spaces credentials:

    export SPACES_ACCESS_KEY_ID=...
    export SPACES_SECRET_ACCESS_KEY=...
"""

from __future__ import annotations

import argparse
import json
import os
import sys
import time

try:
    import boto3
    from botocore.config import Config
    from botocore.exceptions import ClientError
except ImportError:
    boto3 = None


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


def summarise(result: dict) -> dict:
    """The row this run contributes to the index.

    Deliberately small: the index is fetched to answer "what changed", and a
    hundred full results would make that a download rather than a lookup.
    """
    g = result.get("git") or {}
    h = result.get("host") or {}
    row = {
        "started": result.get("started"),
        "finished": result.get("finished"),
        "branch": g.get("branch"),
        "commit": g.get("commit"),
        "short": g.get("short"),
        "subject": g.get("subject"),
        "dirty": g.get("dirty"),
        "host": {"cpu": h.get("cpu"), "cpu_count": h.get("cpu_count"), "memory_gb": h.get("memory_gb")},
        "dataset_sha256": (result.get("dataset") or {}).get("sha256"),
        "knee": {},
        "p50": {},
    }
    for phase in result.get("knee") or []:
        for e in phase.get("entries") or []:
            if e.get("knee", {}).get("knee_rps"):
                row["knee"][f"{e['server']}/{phase['suite']}"] = round(e["knee"]["knee_rps"], 1)
    for phase in result.get("closed") or []:
        for e in phase.get("entries") or []:
            if e.get("summary"):
                key = f"{e['server']}/{phase['suite']}/c{phase['concurrency']}"
                row["p50"][key] = round(e["summary"]["service_ms"]["p50"], 3)
    return row


def key_for(result: dict, stamp: str) -> str:
    g = result.get("git") or {}
    branch = (g.get("branch") or "detached").replace("/", "_")
    commit = g.get("commit") or "unknown"
    return f"results/{branch}/{commit}/{stamp}.json"


def provenance_problem(result: dict) -> str | None:
    """Why this run cannot be filed under a commit, or None if it can.

    The results tree is addressed by branch and commit, so a run that does not
    know its own commit has nowhere correct to go. Until suite.py grew
    `available`, a box without `.git` reported `dirty: False` with a null
    commit, and that sailed through the dirty check as if it were clean.
    """
    g = result.get("git") or {}
    if not g.get("available"):
        return (g.get("reason")
                or "the run recorded no git metadata, so it is not attributable to a commit")
    if not g.get("commit"):
        return "the run reports git as available but carries no commit"
    if g.get("dirty") is None:
        return "the run does not say whether the working tree was clean"
    return None


def rebuild_index(s3, bucket: str, verbose: bool = True) -> dict:
    index = {"generated": time.strftime("%Y-%m-%dT%H:%M:%S"), "runs": []}
    paginator = s3.get_paginator("list_objects_v2")
    keys = []
    for page in paginator.paginate(Bucket=bucket, Prefix="results/"):
        for obj in page.get("Contents", []):
            if obj["Key"].endswith(".json"):
                keys.append(obj["Key"])
    if verbose:
        print(f"indexing {len(keys)} result object(s)")
    for k in sorted(keys):
        try:
            body = s3.get_object(Bucket=bucket, Key=k)["Body"].read()
            row = summarise(json.loads(body))
        except (ClientError, json.JSONDecodeError, KeyError) as e:
            print(f"  skipping {k}: {e}", file=sys.stderr)
            continue
        row["key"] = k
        index["runs"].append(row)
    index["runs"].sort(key=lambda r: r.get("started") or "", reverse=True)
    s3.put_object(Bucket=bucket, Key="index.json", Body=json.dumps(index, indent=2).encode(),
                  ContentType="application/json", ACL="private", CacheControl="no-cache")
    return index


def compare(index: dict, baseline_branch: str, current: dict | None) -> str:
    """Latest clean run on `baseline_branch` against the newest run overall."""
    runs = index.get("runs") or []
    base = next((r for r in runs if r.get("branch") == baseline_branch and not r.get("dirty")), None)
    head = current or (runs[0] if runs else None)
    if not base or not head:
        return f"no comparable pair (baseline on {baseline_branch}: {'yes' if base else 'no'})"
    if base.get("dataset_sha256") != head.get("dataset_sha256"):
        return ("refusing to compare: the two runs used different datasets\n"
                f"  {baseline_branch}: {base.get('dataset_sha256')}\n"
                f"  current:           {head.get('dataset_sha256')}")
    if (base.get("host") or {}).get("cpu") != (head.get("host") or {}).get("cpu"):
        return ("refusing to compare: different CPUs\n"
                f"  {baseline_branch}: {(base.get('host') or {}).get('cpu')}\n"
                f"  current:           {(head.get('host') or {}).get('cpu')}")

    lines = [f"{head.get('branch')}@{head.get('short')} vs {baseline_branch}@{base.get('short')}", ""]
    for label, field, better_is in (("sustained req/s", "knee", "higher"), ("p50 ms", "p50", "lower")):
        keys = sorted(set(base.get(field, {})) & set(head.get(field, {})))
        if not keys:
            continue
        lines += [f"  {label}:"]
        for k in keys:
            b, h = base[field][k], head[field][k]
            if not b:
                continue
            delta = (h - b) / b * 100.0
            improved = delta > 0 if better_is == "higher" else delta < 0
            mark = "" if abs(delta) < 5 else ("  ok" if improved else "  REGRESSION")
            lines.append(f"    {k:<40} {b:>10.2f} -> {h:>10.2f}  {delta:+6.1f}%{mark}")
    return "\n".join(lines)


def main() -> None:
    ap = argparse.ArgumentParser(description="publish sabre benchmark results to a Space")
    ap.add_argument("--bucket", required=True)
    ap.add_argument("--region", default="nyc3")
    ap.add_argument("--endpoint", default=None)
    ap.add_argument("--result", help="suite.json to publish")
    ap.add_argument("--index-only", action="store_true", help="rebuild index.json without publishing")
    ap.add_argument("--compare", metavar="BRANCH", help="print a comparison against this branch's latest clean run")
    ap.add_argument("--allow-dirty", action="store_true",
                    help="publish even though the working tree was dirty when measured")
    ap.add_argument("--allow-unattributable", action="store_true",
                    help="publish even though the run does not know which commit it measured")
    args = ap.parse_args()

    s3 = client(args.region, args.endpoint)
    published = None

    if args.result:
        with open(args.result) as f:
            result = json.load(f)
        problem = provenance_problem(result)
        if problem and not args.allow_unattributable:
            sys.exit(f"refusing to publish: {problem}.\n"
                     "A result filed under an unknown commit is worse than no result, because\n"
                     "later runs get compared against it. Re-run with remote-bench.sh, which\n"
                     "captures git metadata locally and passes it to the box, or override with\n"
                     "--allow-unattributable if you really mean to.")
        if (result.get("git") or {}).get("dirty") and not args.allow_dirty:
            sys.exit("the working tree was dirty when this ran, so it is not attributable to a commit.\n"
                     "Publish anyway with --allow-dirty if you meant to.")
        stamp = (result.get("started") or time.strftime("%Y-%m-%dT%H:%M:%S")).replace(":", "-")
        key = key_for(result, stamp)
        s3.put_object(Bucket=args.bucket, Key=key, Body=json.dumps(result).encode(),
                      ContentType="application/json", ACL="private",
                      CacheControl="public, max-age=31536000, immutable")
        print(f"published s3://{args.bucket}/{key}")
        published = summarise(result)

    if args.result or args.index_only:
        index = rebuild_index(s3, args.bucket)
        print(f"index.json: {len(index['runs'])} run(s)")
    else:
        index = json.loads(s3.get_object(Bucket=args.bucket, Key="index.json")["Body"].read())

    if args.compare:
        print()
        print(compare(index, args.compare, published))


if __name__ == "__main__":
    main()
