"""Interrupt actual HTTP transfers between disposable Buddy clients and the real server."""
import http.client
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import socket
import sqlite3
import subprocess
import sys
import tempfile
import threading

from sync import binary, cli


class Faults:
    def __init__(self):
        self.drop_close = None
        self.lost_op = None
        self.replays = 0
        self.block_packs = False
        self.stored_packs = []
        self.deny = False
        self.denials = 0


def chunk_keys(pack):
    keys = set()
    offset = 0
    while offset < len(pack):
        keys.add(pack[offset:offset + 64].decode("ascii"))
        size = int.from_bytes(pack[offset + 64:offset + 68], "big")
        offset += 68 + size
    assert offset == len(pack)
    return keys


def proxy_handler(upstream, faults):
    class Proxy(BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def do_POST(self):
            body = self.rfile.read(int(self.headers["Content-Length"]))
            rpc = json.loads(body) if self.path == "/v1/rpc" else {}
            is_commit = rpc.get("action") == "commit"
            if is_commit and faults.deny:
                faults.denials += 1
                self.send_error(403)
                return
            if self.path == "/v1/packs" and faults.block_packs and faults.stored_packs:
                self.send_error(503)
                return
            headers = {k: v for k, v in self.headers.items() if k.lower() not in ("host", "connection")}
            conn = http.client.HTTPConnection(upstream, timeout=20)
            try:
                conn.request("POST", self.path, body, headers)
                response = conn.getresponse()
                data = response.read()
                status = response.status
            finally:
                conn.close()
            if self.path == "/v1/packs" and 200 <= status < 300:
                faults.stored_packs.append(chunk_keys(body))
            if is_commit and faults.lost_op == rpc["op"]:
                faults.replays += 1
            if is_commit and faults.drop_close is not None and faults.drop_close == rpc["close"]:
                assert status == 200
                faults.lost_op = rpc["op"]
                faults.drop_close = None
                # The upstream transaction has already committed. Break this socket
                # before delivering the headers, precisely at the ambiguous boundary.
                self.connection.shutdown(socket.SHUT_RDWR)
                self.close_connection = True
                return
            self.send_response(status)
            self.send_header("Content-Type", "application/json" if rpc or self.path == "/v1/login" else "application/octet-stream")
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)
    return Proxy


def main(root):
    cfg = root / "server.toml"
    cfg.write_text('listen="127.0.0.1:0"\ndatabase="store.sqlite"\n')
    server = subprocess.Popen([binary, "serve", "--config", cfg], stderr=subprocess.PIPE, text=True)
    proxy = None
    try:
        startup = server.stderr.readline()
        assert startup.startswith("Buddy listening on "), startup
        faults = Faults()
        proxy = ThreadingHTTPServer(("127.0.0.1", 0), proxy_handler(startup.split()[-1], faults))
        threading.Thread(target=proxy.serve_forever, daemon=True).start()
        url = f"http://127.0.0.1:{proxy.server_port}"
        a, b = root / "a", root / "b"
        world, witness = root / "world A", root / "world B"
        for profile, folder in [(a, world), (b, witness)]:
            folder.mkdir()
            invite = ["--invite", cli(profile, "invite", "--config", cfg).strip()] if profile == a else []
            cli(profile, "login", url, "tester", "--device", profile.name, "--private-file-credential",
                "--password-stdin", *invite, input="test-password\n")
            cli(profile, "setup", "world", folder)
        save = world / "level"
        save.write_bytes(b"baseline")
        cli(a, "sync", "world")
        cli(b, "sync", "world")
        with sqlite3.connect(root / "store.sqlite") as db:
            def history():
                return db.execute("select head, (select count(*) from versions) from games").fetchone()

            previous = history()
            for i in range(10):
                (world / f"region-{i}").write_bytes(os.urandom(2 * 1024**2))
            faults.stored_packs.clear()
            faults.block_packs = True
            cli(a, "sync", "world", ok=False)
            assert history() == previous
            assert len(faults.stored_packs) == 1
            already_durable = faults.stored_packs.pop()
            cli(b, "sync", "world")
            assert list(witness.iterdir()) == [witness / "level"]
            faults.block_packs = False
            cli(a, "sync", "world")
            assert faults.stored_packs and all(already_durable.isdisjoint(pack) for pack in faults.stored_packs)
            cli(b, "sync", "world")
            assert all(p.read_bytes() == (witness / p.name).read_bytes() for p in world.iterdir())
            print("PASS interrupted multi-pack upload keeps the old head and resumes missing chunks", flush=True)

            previous = history()
            save.write_text("new pre-launch save")
            faults.drop_close = False
            cli(a, "sync", "world")
            assert faults.replays == 1 and history()[1] == previous[1] + 1
            previous = history()
            faults.drop_close = True
            cli(a, "run", "--game", "world", "--", sys.executable, "-c",
                "from pathlib import Path; import sys; Path(sys.argv[1]).write_text('post-game save')", save)
            assert faults.replays == 2 and history()[1] == previous[1] + 1
            cli(b, "sync", "world")
            assert (witness / "level").read_text() == "post-game save"
            assert "0 queued" in cli(a, "status", "world")
            print("PASS lost commit replies replay the same operation before launch and after lease release", flush=True)

            previous = history()
            faults.deny = True
            save.write_text("locally preserved")
            cli(a, "sync", "world", ok=False)
            assert faults.denials == 1 and history() == previous and save.read_text() == "locally preserved"
            print("PASS an explicit denial is returned once and preserves local progress", flush=True)
    finally:
        if proxy is not None:
            proxy.shutdown()
            proxy.server_close()
        server.terminate()
        server.wait(timeout=10)


if __name__ == "__main__":
    with tempfile.TemporaryDirectory() as temp:
        main(Path(temp))
