#!/usr/bin/env python3
"""A fake of the GitHub releases API of a private repository, for scripts/dist-test.sh.

Serves the releases below ROOT: ROOT/<tag>/<asset files>, and ROOT/latest holding the tag
that /releases/latest returns. Like GitHub for a private repository:

- every /repos/... request without `Authorization: Bearer <token>` (or `token <token>`) gets
  404 Not Found;
- GET /repos/<owner>/<repo>/releases/latest and /releases/tags/<tag> return the release as
  JSON, with each asset's API `url` (.../releases/assets/<id>) and `browser_download_url`;
- GET /repos/<owner>/<repo>/releases/assets/<id> with `Accept: application/octet-stream`
  redirects (302) to /objects/<tag>/<name>, which serves the file without a token (as
  GitHub's object storage does with a signed URL); with another Accept header it returns
  the asset's JSON.

Every release is long, as a real one with long notes and many assets can be: its body is
over 100 KB (after the assets, as GitHub orders the keys) and it lists hundreds of further
assets of other names around the real ones, so that a client that stops reading the JSON at
the first match (and so kills its writer with SIGPIPE under pipefail) fails the test.

Every request is appended to ROOT/../requests.log (method, path, whether a token was sent).

Usage: fake_github.py ROOT PORT TOKEN [OWNER/REPO]
"""

import json
import os
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import unquote, urlsplit

ROOT, PORT, TOKEN = sys.argv[1], int(sys.argv[2]), sys.argv[3]
REPO = sys.argv[4] if len(sys.argv) > 4 else "kaito4681/gpueue"
# See above: a long release. The notes mention assets, URLs and tags the way real notes do.
PADDING_ASSETS = 300
BODY = "".join(
    f"- change {i}, see {{the notes}}: \"tag_name\": \"v0.0.{i}\", "
    f"https://github.com/{REPO}/releases/download/v0.0.{i}/gpueue.tar.gz\n"
    for i in range(1500)
)
assert len(BODY) > 100_000
LOG = os.path.join(os.path.dirname(os.path.abspath(ROOT)), "requests.log")


def tags():
    return sorted(
        d for d in os.listdir(ROOT) if os.path.isdir(os.path.join(ROOT, d))
    )


def assets(tag):
    """(id, name) of the files of a release; ids are stable for a tag and name."""
    names = sorted(os.listdir(os.path.join(ROOT, tag)))
    base = (tags().index(tag) + 1) * 1000
    return [(base + i, name) for i, name in enumerate(names)]


def find_asset(asset_id):
    for tag in tags():
        for aid, name in assets(tag):
            if aid == asset_id:
                return tag, name
    return None


class Handler(BaseHTTPRequestHandler):
    # Speak HTTP/1.1 with persistent connections, like GitHub. With the default HTTP/1.0 the
    # server closes the connection after every response but does not say so with
    # `Connection: close`; a client that pools the connection (ureq does, as the response has
    # a Content-Length) then sends the next request, e.g. the one following the 302 of an
    # asset download, on a socket the server is closing, and fails with "Peer disconnected".
    # Every response below carries a Content-Length, as HTTP/1.1 keep-alive requires.
    protocol_version = "HTTP/1.1"

    def log_message(self, fmt, *args):  # quiet; see requests.log
        pass

    def base(self):
        return f"http://{self.headers.get('Host', f'127.0.0.1:{PORT}')}"

    def asset_json(self, tag, aid, name):
        asset = self.asset_json_fields(tag, aid, name)
        asset["size"] = os.path.getsize(os.path.join(ROOT, tag, name))
        return asset

    def asset_json_fields(self, tag, aid, name):
        base = self.base()
        return {
            "url": f"{base}/repos/{REPO}/releases/assets/{aid}",
            "id": aid,
            "node_id": f"RA_{aid}",
            "name": name,
            "label": "",
            "uploader": {"login": "github-actions[bot]", "id": 41898282,
                         "url": f"{base}/users/github-actions%5Bbot%5D"},
            "content_type": "application/octet-stream",
            "state": "uploaded",
            "size": 0,
            "browser_download_url":
                f"https://github.com/{REPO}/releases/download/{tag}/{name}",
        }

    def release_json(self, tag):
        base = self.base()
        real = [self.asset_json(tag, aid, name) for aid, name in assets(tag)]
        # Listed, but not downloadable (find_asset does not know their ids).
        padding = [self.asset_json_fields(tag, 900000 + i, f"notes-{i:03}.txt")
                   for i in range(PADDING_ASSETS)]
        half = PADDING_ASSETS // 2
        return {
            "url": f"{base}/repos/{REPO}/releases/1",
            "assets_url": f"{base}/repos/{REPO}/releases/1/assets",
            "id": 1,
            "tag_name": tag,
            "name": tag,
            "draft": False,
            "prerelease": False,
            "assets": padding[:half] + real + padding[half:],
            "body": BODY,
        }

    def send(self, code, body=b"", content_type="application/json", headers=()):
        self.send_response(code)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        for key, value in headers:
            self.send_header(key, value)
        self.end_headers()
        if self.command != "HEAD":
            self.wfile.write(body)

    def send_json(self, code, value):
        self.send(code, json.dumps(value).encode())

    def not_found(self):
        self.send_json(404, {"message": "Not Found",
                             "documentation_url": "https://docs.github.com/rest"})

    def do_HEAD(self):
        self.do_GET()

    def do_GET(self):
        path = unquote(urlsplit(self.path).path)
        auth = self.headers.get("Authorization", "")
        authorized = auth in (f"Bearer {TOKEN}", f"token {TOKEN}")
        with open(LOG, "a") as log:
            log.write(f"{self.command} {path} token={'yes' if auth else 'no'}"
                      f" accept={self.headers.get('Accept', '')}\n")

        if path.startswith("/objects/"):
            parts = path.split("/")
            if len(parts) == 4 and parts[2] in tags():
                file = os.path.join(ROOT, parts[2], parts[3])
                if os.path.isfile(file):
                    with open(file, "rb") as f:
                        return self.send(200, f.read(), "application/octet-stream")
            return self.not_found()

        prefix = f"/repos/{REPO}/releases/"
        if not path.startswith(prefix) or not authorized:
            return self.not_found()
        rest = path[len(prefix):]
        if rest == "latest":
            latest_file = os.path.join(ROOT, "latest")
            if not os.path.exists(latest_file):
                return self.not_found()
            with open(latest_file) as f:
                tag = f.read().strip()
            return self.send_json(200, self.release_json(tag))
        if rest.startswith("tags/"):
            tag = rest[len("tags/"):]
            if tag not in tags():
                return self.not_found()
            return self.send_json(200, self.release_json(tag))
        if rest.startswith("assets/") and rest[len("assets/"):].isdigit():
            found = find_asset(int(rest[len("assets/"):]))
            if found is None:
                return self.not_found()
            tag, name = found
            if self.headers.get("Accept") == "application/octet-stream":
                location = f"{self.base()}/objects/{tag}/{name}"
                return self.send(302, b"", "text/plain", [("Location", location)])
            aid = int(rest[len("assets/"):])
            return self.send_json(200, self.asset_json(tag, aid, name))
        return self.not_found()


if __name__ == "__main__":
    ThreadingHTTPServer(("127.0.0.1", PORT), Handler).serve_forever()
