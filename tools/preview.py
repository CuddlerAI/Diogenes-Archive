#!/usr/bin/env python3
"""Local preview server for website/.

`python3 -m http.server` is not good enough here: the site relies on Cloudflare
Workers Static Assets `html_handling` (auto-trailing-slash), so /research is
served from research.html and /models from models/index.html. Plain http.server
404s both, which makes every inner page look broken locally. This mimics that
resolution plus the _redirects file, so what you see locally is what deploys.

    python3 tools/preview.py            # serves website/ on http://localhost:8788
    python3 tools/preview.py website 8000
"""
import http.server
import os
import socketserver
import sys

ROOT = os.path.abspath(sys.argv[1] if len(sys.argv) > 1 else "website")
PORT = int(sys.argv[2]) if len(sys.argv) > 2 else 8788

REDIRECTS = {}
_rp = os.path.join(ROOT, "_redirects")
if os.path.exists(_rp):
    with open(_rp) as fh:
        for line in fh:
            line = line.strip()
            if not line or line.startswith("#"):
                continue
            parts = line.split()
            if len(parts) >= 2:
                REDIRECTS[parts[0]] = (parts[1], int(parts[2]) if len(parts) > 2 else 302)


class Handler(http.server.SimpleHTTPRequestHandler):
    def __init__(self, *args, **kwargs):
        super().__init__(*args, directory=ROOT, **kwargs)

    def do_GET(self):
        path = self.path.split("?")[0].split("#")[0]
        if path in REDIRECTS:
            target, code = REDIRECTS[path]
            self.send_response(code)
            self.send_header("Location", target)
            self.end_headers()
            return
        rel = path.lstrip("/")
        if rel and not os.path.exists(os.path.join(ROOT, rel)):
            for candidate in (rel + ".html", os.path.join(rel, "index.html")):
                if os.path.exists(os.path.join(ROOT, candidate)):
                    self.path = "/" + candidate
                    break
        return super().do_GET()

    def log_message(self, *args):
        pass


if __name__ == "__main__":
    socketserver.TCPServer.allow_reuse_address = True
    with socketserver.TCPServer(("", PORT), Handler) as httpd:
        print(f"serving {ROOT} on http://localhost:{PORT}  (ctrl-c to stop)")
        httpd.serve_forever()
