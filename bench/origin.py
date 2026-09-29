#!/usr/bin/env python3
"""A range-capable static file server for running the benchmark without Docker.

Serves the files in a directory under the same two prefixes as the nginx
origin, /sabre/<file> and /titiler/<file>, answers `Range: bytes=a-b` with a
206, keeps connections alive, and writes the same access log line as
nginx.conf so run.py can account for origin traffic either way.

    python3 bench/origin.py --data bench/data --port 8081 --log bench/out/logs/access.log

Python's built-in http.server does not support range requests, which is why
this exists; it is a benchmark fixture, not a web server.
"""

from __future__ import annotations

import argparse
import os
import re
import threading
import time
from http import HTTPStatus
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PREFIXES = ("/sabre/", "/titiler/")
RANGE = re.compile(r"^bytes=(\d*)-(\d*)$")


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    data_dir = "."
    log_file = None
    log_lock = threading.Lock()

    # ── Requests ──────────────────────────────────────────────────────────────

    def do_GET(self) -> None:
        self.serve(send_body=True)

    def do_HEAD(self) -> None:
        self.serve(send_body=False)

    def serve(self, send_body: bool) -> None:
        if self.path == "/healthz":
            self.reply(HTTPStatus.OK, b"ok\n", send_body)
            return
        path = self.resolve()
        if path is None:
            self.reply(HTTPStatus.NOT_FOUND, b"not found\n", send_body)
            return
        size = os.path.getsize(path)
        start, end = 0, size - 1
        status = HTTPStatus.OK
        header = self.headers.get("Range")
        if header:
            m = RANGE.match(header.strip())
            if not m:
                self.reply(HTTPStatus.BAD_REQUEST, b"bad range\n", send_body)
                return
            first, last = m.group(1), m.group(2)
            if first == "" and last == "":
                self.reply(HTTPStatus.BAD_REQUEST, b"bad range\n", send_body)
                return
            if first == "":                       # suffix range: last N bytes
                start = max(0, size - int(last))
            else:
                start = int(first)
                end = min(int(last), size - 1) if last else size - 1
            if start >= size or start > end:
                self.send_response(HTTPStatus.REQUESTED_RANGE_NOT_SATISFIABLE)
                self.send_header("Content-Range", f"bytes */{size}")
                self.send_header("Content-Length", "0")
                self.end_headers()
                self.log_line(HTTPStatus.REQUESTED_RANGE_NOT_SATISFIABLE, 0)
                return
            status = HTTPStatus.PARTIAL_CONTENT
        length = end - start + 1
        self.send_response(status)
        self.send_header("Content-Type", "image/tiff")
        self.send_header("Accept-Ranges", "bytes")
        self.send_header("Content-Length", str(length))
        if status == HTTPStatus.PARTIAL_CONTENT:
            self.send_header("Content-Range", f"bytes {start}-{end}/{size}")
        self.end_headers()
        sent = 0
        if send_body:
            with open(path, "rb") as f:
                f.seek(start)
                remaining = length
                while remaining > 0:
                    chunk = f.read(min(remaining, 1 << 20))
                    if not chunk:
                        break
                    self.wfile.write(chunk)
                    sent += len(chunk)
                    remaining -= len(chunk)
        self.log_line(status, sent)

    def resolve(self) -> str | None:
        """Map /sabre/<file> or /titiler/<file> to a file inside the data dir."""
        uri = self.path.split("?", 1)[0]
        for prefix in PREFIXES:
            if uri.startswith(prefix):
                rel = uri[len(prefix):]
                break
        else:
            return None
        root = os.path.realpath(self.data_dir)
        path = os.path.realpath(os.path.join(root, rel))
        if not path.startswith(root + os.sep) or not os.path.isfile(path):
            return None
        return path

    def reply(self, status: HTTPStatus, body: bytes, send_body: bool) -> None:
        self.send_response(status)
        self.send_header("Content-Type", "text/plain")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        if send_body:
            self.wfile.write(body)
        self.log_line(status, len(body) if send_body else 0)

    # ── Logging ───────────────────────────────────────────────────────────────

    def log_line(self, status: int, sent: int) -> None:
        line = '{:.3f} {} "{}" {} {} "{}" "{}"\n'.format(
            time.time(), self.command, self.path, int(status), sent,
            self.headers.get("Range", "-"), self.headers.get("User-Agent", "-"))
        if self.log_file is None:
            return
        with self.log_lock:
            self.log_file.write(line)
            self.log_file.flush()

    def log_message(self, format: str, *args) -> None:  # silence the default stderr log
        pass


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--data", default=os.path.join(os.path.dirname(__file__), "data"))
    ap.add_argument("--port", type=int, default=8081)
    ap.add_argument("--bind", default="127.0.0.1")
    ap.add_argument("--log", default=os.path.join(os.path.dirname(__file__), "out", "logs", "access.log"))
    args = ap.parse_args()

    os.makedirs(os.path.dirname(os.path.abspath(args.log)), exist_ok=True)
    Handler.data_dir = args.data
    Handler.log_file = open(args.log, "a")
    server = ThreadingHTTPServer((args.bind, args.port), Handler)
    server.daemon_threads = True
    print(f"origin serving {os.path.abspath(args.data)} on http://{args.bind}:{args.port}/{{sabre,titiler}}/<file>")
    print(f"access log: {os.path.abspath(args.log)}")
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()
