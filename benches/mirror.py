#!/usr/bin/env python3
"""A replay mirror of the npm registry, for `benches/bench.sh`.

The benchmark used to make roughly 22,600 anonymous requests to
registry.npmjs.org per default run, which is both a rate limit waiting to
happen and the reason its medians could not be reproduced: retry noise from
live weather was folded into every number. This serves the same packuments and
tarballs from a recording on disk instead, so a measuring run touches nothing
but localhost.

This is bench tooling at the `RegistryClient` seam, *not* an offline mode for
jerky (that is issue #80). Nothing here ships or is reachable from the binary;
`bench.sh` points jerky at it with `JERKY_REGISTRY_URL`, the same override the
integration tests already use.

    mirror.py serve DIR [--port N] [--port-file F]
    mirror.py serve DIR --record [--upstream URL] ...
    mirror.py check DIR

Recording is a separate, deliberate step, in the spirit of `--pin`: it
invalidates every number taken before it, so it belongs in a diff rather than
inside a measuring run. `--record` turns the mirror into a caching proxy — a
miss is fetched from upstream, written down verbatim, and served — and
`bench.sh --record` drives one live install per fixture through it. Recording
by observation rather than from a package list is what makes a recording
complete by construction: whatever an install asks for is what gets stored.

Tarball URLs
------------
`dist.tarball` in a packument is an absolute URL at the upstream registry, so
something has to redirect it at the mirror. Of the two options, this takes the
second: **the mirror rewrites packuments as it serves them**, replacing the
upstream origin with its own. The recording therefore holds verbatim upstream
bytes and is independent of the port the mirror happens to bind, so one
recording replays anywhere and on any machine. Rewriting at record time would
bake a host and port into 470 MB of data; teaching the client to resolve
`dist.tarball` against the base URL would change `RegistryClient`'s contract in
shipped code for the benefit of a benchmark. Tarball *bytes* are never touched,
so integrity still verifies.
"""

import argparse
import json
import os
import socketserver
import sys
import threading
import time
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import unquote

DEFAULT_UPSTREAM = "https://registry.npmjs.org"

# The abbreviated form. The unabbreviated document for a popular package is
# megabytes of every version ever published, and jerky asks for this one, so
# recording anything else would be recording a document it never reads.
ABBREVIATED = "application/vnd.npm.install-v1+json"

RECORDING_JSON = "recording.json"


def die(message):
    sys.stderr.write("mirror: %s\n" % message)
    raise SystemExit(1)


def encode_name(name):
    """`@babel/core` -> `@babel%2fcore`, which is both how jerky asks for it
    and a filename with no directory separator in it."""
    return name.replace("/", "%2f")


def safe_relative(path):
    """Reject anything that could climb out of the recording directory.

    These paths come from our own rewrite of recorded data rather than from a
    hostile client, but a mirror that will happily serve `../../etc/passwd` is
    a mirror nobody should run, recorded data or not.
    """
    path = path.lstrip("/")
    if not path:
        return None
    for part in path.split("/"):
        if part in ("", ".", ".."):
            return None
    return path


class Recording:
    """The bytes on disk, and how a request path maps onto them."""

    def __init__(self, root, upstream=None, record=False):
        self.root = os.path.abspath(root)
        self.record = record
        self.locks = {}
        self.locks_guard = threading.Lock()

        meta_path = os.path.join(self.root, RECORDING_JSON)
        meta = {}
        if os.path.exists(meta_path):
            with open(meta_path) as handle:
                meta = json.load(handle)

        # An existing recording's upstream wins over the flag's default: the
        # rewrite has to name the origin the recorded bytes actually carry, and
        # disagreeing with it would serve packuments whose tarball URLs still
        # point at the live registry.
        self.upstream = (upstream or meta.get("upstream") or DEFAULT_UPSTREAM).rstrip("/")

        if record:
            os.makedirs(os.path.join(self.root, "packuments"), exist_ok=True)
            os.makedirs(os.path.join(self.root, "tarballs"), exist_ok=True)
            meta.update(
                upstream=self.upstream,
                recorded_at=time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
            )
            with open(meta_path, "w") as handle:
                json.dump(meta, handle, indent=2, sort_keys=True)
                handle.write("\n")

    def lock_for(self, key):
        """One lock per key, so sixteen concurrent fetches of one packument
        become one upstream request rather than sixteen."""
        with self.locks_guard:
            lock = self.locks.get(key)
            if lock is None:
                lock = self.locks[key] = threading.Lock()
            return lock

    def packument_file(self, name):
        return os.path.join(self.root, "packuments", encode_name(name) + ".json")

    def tarball_file(self, url_path):
        relative = safe_relative(url_path)
        if relative is None:
            return None
        return os.path.join(self.root, "tarballs", relative)

    def write(self, path, body):
        """Write through a temporary name, so a mirror killed mid-fetch leaves
        no half a tarball that a later run would serve as if it were whole."""
        os.makedirs(os.path.dirname(path), exist_ok=True)
        temporary = "%s.%d.part" % (path, os.getpid())
        with open(temporary, "wb") as handle:
            handle.write(body)
        os.replace(temporary, path)

    def fetch(self, url, accept=None):
        headers = {"user-agent": "jerky-bench-mirror"}
        if accept:
            headers["accept"] = accept
        request = urllib.request.Request(url, headers=headers)
        with urllib.request.urlopen(request, timeout=120) as response:
            return response.read()

    def counts(self):
        """Packuments, tarballs, and bytes on disk. `.part` files are the
        remains of an interrupted fetch and are not part of the recording."""
        packuments = tarballs = 0
        total_bytes = 0
        for kind in ("packuments", "tarballs"):
            for dirpath, _, filenames in os.walk(os.path.join(self.root, kind)):
                for name in filenames:
                    if name.endswith(".part"):
                        continue
                    total_bytes += os.path.getsize(os.path.join(dirpath, name))
                    if kind == "packuments":
                        packuments += 1
                    else:
                        tarballs += 1
        return packuments, tarballs, total_bytes


class Server(ThreadingHTTPServer):
    """A mirror that does not narrate its own disconnections.

    jerky holds sixteen keep-alive connections open and drops them when an
    install finishes, which `socketserver` reports as an unhandled
    `ConnectionResetError` — a twenty-line traceback per connection, into the
    same log a genuine miss has to be found in. Nothing has gone wrong, so
    nothing is printed.
    """

    daemon_threads = True

    def server_bind(self):
        """Bind without asking the resolver who we are.

        `HTTPServer.server_bind` follows the bind with `socket.getfqdn()` on
        the address just bound, to fill in `server_name`. That is a reverse DNS
        lookup, and on a machine whose resolver has nothing to say about
        `127.0.0.1` it blocks for as long as the resolver takes to give up —
        tens of seconds on a macOS CI runner, where it made the mirror look
        like it had hung: the port file is written after this returns, so the
        harness timed out against a process that was alive, silent, and stuck
        inside `__init__`.

        Nothing here wants the name. `server_name` and `server_port` are read
        by the CGI handler to fill in environment variables, and this serves
        packuments and tarballs. So the bound address is the answer, and no
        question is asked.
        """
        socketserver.TCPServer.server_bind(self)
        self.server_name, self.server_port = self.server_address[:2]

    def handle_error(self, request, client_address):
        if isinstance(sys.exc_info()[1], (ConnectionResetError, BrokenPipeError)):
            return
        ThreadingHTTPServer.handle_error(self, request, client_address)


class Handler(BaseHTTPRequestHandler):
    # Nagle off, and this is not a micro-optimisation — it is the difference
    # between measuring jerky and measuring a TCP stall.
    #
    # `BaseHTTPRequestHandler` leaves Nagle's algorithm on, and a response goes
    # out as a header write followed by a body write. Nagle holds the second
    # write until the first is acknowledged, the client has nothing to say
    # until it has the body, and its delayed-ACK timer holds the
    # acknowledgement back — so the two wait for each other, tens of
    # milliseconds at a time, several hundred times a run.
    #
    # Measured on the `alotta-packages` cold row: 13.1s with Nagle on, 2.7s
    # with it off, on the same recording and the same binary. It was also
    # bimodal, landing at 13.1s or ~5s depending on whether a run's write
    # pattern happened to trip the stall, which is what made the cold row
    # unusable for comparing two binaries. Neither side was busy — jerky spent
    # ~4.9s of CPU either way and the mirror ~0.9s, the rest was two processes
    # waiting on each other.
    disable_nagle_algorithm = True

    # Keep-alive, because jerky holds sixteen connections open and a mirror
    # that closed each one would have the benchmark measuring TCP setup.
    protocol_version = "HTTP/1.1"

    recording = None
    base_url = None

    # Seconds to wait before answering, modelling a registry that is not on
    # this machine. Zero is the default and the historical behaviour.
    delay = 0.0

    def log_message(self, *_args):
        """Silence the per-request access log: thousands of lines of it cost
        real time in the middle of something being timed."""

    def handle_one_request(self):
        try:
            BaseHTTPRequestHandler.handle_one_request(self)
        except (ConnectionResetError, BrokenPipeError):
            # The client hung up. See `Server.handle_error`.
            self.close_connection = True

    def note(self, message):
        sys.stderr.write("mirror: %s\n" % message)
        sys.stderr.flush()

    def send_bytes(self, status, body, content_type):
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def miss(self, what):
        """A miss is a 404 rather than a 5xx deliberately: jerky retries 5xx
        three times with backoff and does not retry a 404, so this fails fast
        and says why instead of being slow at being wrong."""
        body = (
            "not in the recording: %s\n"
            "Run `./benches/bench.sh --record` to seed the mirror.\n" % what
        ).encode()
        self.note("MISS %s (recording incomplete; run ./benches/bench.sh --record)" % what)
        self.send_bytes(404, body, "text/plain")

    def do_GET(self):  # noqa: N802 - the name is BaseHTTPRequestHandler's
        # Before the response is built, so it is a *round trip* being modelled
        # rather than a slow disk. A recording served from loopback out of the
        # page cache answers in well under a millisecond, which makes the
        # mirror an excellent instrument for anything CPU-bound and a useless
        # one for anything whose cost is waiting — there is no latency present
        # for an overlap, a cache or a pipeline to remove. Real registries sit
        # tens of milliseconds away and that is the regime those changes are
        # for, so it has to be expressible here or their effect cannot be
        # measured at all.
        #
        # Latency only, and deliberately not bandwidth. A delay per request is
        # one number with one meaning, and it is the half that separates a
        # design that overlaps its waiting from one that does not. Throttling
        # bytes as well would model a real link more closely and would make
        # every row depend on a second invented constant.
        if self.delay:
            time.sleep(self.delay)

        path = self.path.split("?", 1)[0]
        try:
            if path.endswith(".tgz"):
                self.serve_tarball(path)
            else:
                self.serve_metadata(path)
        except BrokenPipeError:
            pass

    def do_HEAD(self):  # noqa: N802
        self.do_GET()

    def serve_tarball(self, path):
        target = self.recording.tarball_file(path)
        if target is None:
            self.send_bytes(400, b"bad path\n", "text/plain")
            return

        if not os.path.exists(target) and self.recording.record:
            with self.recording.lock_for(path):
                if not os.path.exists(target):
                    url = self.recording.upstream + path
                    try:
                        self.recording.write(target, self.recording.fetch(url))
                    except (urllib.error.URLError, urllib.error.HTTPError) as err:
                        self.note("upstream %s: %s" % (url, err))
                        self.send_bytes(502, b"upstream failed\n", "text/plain")
                        return

        if not os.path.exists(target):
            self.miss(path)
            return

        with open(target, "rb") as handle:
            body = handle.read()
        self.send_bytes(200, body, "application/octet-stream")

    def serve_metadata(self, path):
        decoded = unquote(path).lstrip("/")
        if not decoded:
            self.send_bytes(200, b"jerky bench mirror\n", "text/plain")
            return

        body = self.packument(decoded)
        if body is not None:
            self.send_bytes(200, self.rewrite(body), "application/json")
            return

        # `/lodash/4.17.21`, `/@babel%2fcore/7.29.7`: a version manifest, which
        # the mirror answers out of the packument it already has rather than
        # recording a second copy of the same bytes. jerky's bare install does
        # not use this endpoint, but `jerky install <name>` does, and a mirror
        # that 404s it would be a confusing thing to hand someone.
        if "/" in decoded:
            name, _, version = decoded.rpartition("/")
            body = self.packument(name)
            if body is not None:
                served = self.version_metadata(body, name, version)
                if served is None:
                    self.miss("%s@%s" % (name, version))
                else:
                    self.send_bytes(200, self.rewrite(served), "application/json")
                return

        self.miss(decoded)

    def packument(self, name):
        target = self.recording.packument_file(name)
        if not os.path.exists(target) and self.recording.record:
            with self.recording.lock_for(name):
                if not os.path.exists(target):
                    url = "%s/%s" % (self.recording.upstream, encode_name(name))
                    try:
                        fetched = self.recording.fetch(url, accept=ABBREVIATED)
                    except urllib.error.HTTPError as err:
                        if err.code != 404:
                            self.note("upstream %s: %s" % (url, err))
                        return None
                    except urllib.error.URLError as err:
                        self.note("upstream %s: %s" % (url, err))
                        return None
                    self.check_origins(name, fetched)
                    self.recording.write(target, fetched)

        if not os.path.exists(target):
            return None
        with open(target, "rb") as handle:
            return handle.read()

    def check_origins(self, name, body):
        """Warn at record time about a tarball URL the serve-time rewrite will
        not catch.

        The rewrite is a substitution of one origin, which is all npm needs.
        Parsing every packument as JSON to be certain would be a cost paid on
        every measured request; paying it once here, where nothing is being
        timed, is the trade.
        """
        try:
            document = json.loads(body)
        except ValueError:
            return
        for version in (document.get("versions") or {}).values():
            url = (version.get("dist") or {}).get("tarball")
            if url and not url.startswith(self.recording.upstream + "/"):
                self.note("%s: tarball outside %s: %s" % (name, self.recording.upstream, url))
                return

    def version_metadata(self, packument_body, name, version):
        try:
            document = json.loads(packument_body)
        except ValueError:
            return None
        versions = document.get("versions") or {}
        if version not in versions:
            version = (document.get("dist-tags") or {}).get(version)
            if version not in versions:
                return None
        entry = dict(versions[version])
        entry.setdefault("name", name)
        entry.setdefault("version", version)
        return json.dumps(entry).encode()

    def rewrite(self, body):
        """Point `dist.tarball` at this mirror. See the module docstring for
        why this happens here rather than at record time."""
        return body.replace(self.recording.upstream.encode(), self.base_url.encode())


def serve(args):
    recording = Recording(args.dir, upstream=args.upstream, record=args.record)
    if not args.record:
        packuments, tarballs, _ = recording.counts()
        if packuments == 0 and tarballs == 0:
            die("%s holds no recording. Run `./benches/bench.sh --record` first." % recording.root)

    server = Server(("127.0.0.1", args.port), Handler)
    port = server.server_address[1]

    Handler.recording = recording
    Handler.base_url = "http://127.0.0.1:%d" % port
    Handler.delay = max(0.0, getattr(args, "delay", 0.0)) / 1000.0

    if args.port_file:
        # Written after the bind and through a rename, so a reader that sees
        # the file at all sees a port that is already accepting connections.
        temporary = args.port_file + ".part"
        with open(temporary, "w") as handle:
            handle.write("%d\n" % port)
        os.replace(temporary, args.port_file)

    sys.stderr.write(
        "mirror: serving %s on %s%s%s\n"
        % (
            recording.root,
            Handler.base_url,
            " (recording)" if args.record else "",
            " (+%gms per request)" % (Handler.delay * 1000) if Handler.delay else "",
        )
    )
    sys.stderr.flush()
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    return 0


def check(args):
    recording = Recording(args.dir)
    packuments, tarballs, total = recording.counts()
    sys.stdout.write("%d packuments, %d tarballs, %s\n" % (packuments, tarballs, human(total)))
    return 0 if packuments or tarballs else 1


def human(count):
    for unit in ("B", "KB", "MB", "GB"):
        if count < 1024 or unit == "GB":
            return "%.1f%s" % (count, unit)
        count /= 1024.0
    raise AssertionError("unreachable")


def main(argv):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    sub = parser.add_subparsers(dest="command", required=True)

    serve_parser = sub.add_parser("serve", help="serve a recording")
    serve_parser.add_argument("dir")
    serve_parser.add_argument("--port", type=int, default=0, help="0 picks a free one")
    serve_parser.add_argument("--port-file", help="write the bound port here once listening")
    serve_parser.add_argument(
        "--record", action="store_true", help="fetch and write down what is missing"
    )
    serve_parser.add_argument(
        "--upstream", default=None, help="registry to record from (default %s)" % DEFAULT_UPSTREAM
    )
    serve_parser.add_argument(
        "--delay",
        type=float,
        default=0.0,
        metavar="MS",
        help="wait this many milliseconds before answering each request, "
        "modelling a registry that is not on this machine",
    )
    serve_parser.set_defaults(run=serve)

    check_parser = sub.add_parser("check", help="report what a recording holds")
    check_parser.add_argument("dir")
    check_parser.set_defaults(run=check)

    args = parser.parse_args(argv)
    return args.run(args)


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
