"""Exercise HTTP sync with isolated profiles. Optional second argument: local IP to bind."""
import ipaddress
import json
import os
from pathlib import Path
import secrets
import sqlite3
import subprocess
import sys
import tempfile
import time
import urllib.request

binary = str(Path(sys.argv[1]).resolve(strict=True))


def cli(home, *args, input=None, ok=True, extra=None):
    r = subprocess.run([binary, *map(str, args)], input=input, text=True, capture_output=True,
                       timeout=50, env=dict(os.environ, BUDDY_HOME=str(home), **(extra or {})))
    if ok and r.returncode:
        raise AssertionError(f"{args[0]} failed: {r.stderr}")
    if not ok:
        assert r.returncode, args
    return r.stdout if ok else r.stderr


def post(url, route, value, token=""):
    data = json.dumps(value).encode()
    req = urllib.request.Request(url + route, data=data,
                                 headers={"Content-Type": "application/json", "Authorization": "Bearer " + token})
    with urllib.request.urlopen(req, timeout=6) as r:
        return json.load(r)


def main(root, address):
    config = root / "server.toml"
    init = [binary, "server", "init"]
    subprocess.run(init, cwd=root, check=True, capture_output=True)
    example = Path(__file__).resolve().parents[1] / "server.example.toml"
    assert config.read_bytes() == example.read_bytes()
    host = f"[{address}]" if address.version == 6 else str(address)
    settings = f'listen = "{host}:0"\ndatabase = "state.sqlite"\n'
    config.write_text(settings)
    existing = subprocess.run(init, cwd=root, capture_output=True, text=True)
    assert existing.returncode != 0 and existing.stderr.strip() == "Buddy: server.toml already exists"
    assert config.read_text() == settings
    user_a, user_b, other = [root / a for a in ("client-a", "client-b", "other")]
    env = {"BUDDY_ALLOW_FILE_CREDENTIAL": "true"}
    password = secrets.token_urlsafe(24)
    server = subprocess.Popen([binary, "server", "serve"], cwd=root, stderr=subprocess.PIPE, text=True)
    try:
        line = server.stderr.readline().strip()
        assert line.startswith("Buddy listening on "), line
        listener = line.split()[-1]
        assert ipaddress.ip_address(listener.rsplit(":", 1)[0].strip("[]")) == address
        url = "http://" + listener
        for profile, name, host, new in [(user_a, "player", "desktop", True), (user_b, "player", "deck", False), (other, "guest", "guest-device", True)]:
            args = ["login", url, name, "--device", host, "--password-stdin"]
            if new:
                args += ["--invite", cli(profile, "server", "invite", "--config", config).strip()]
            cli(profile, *args, input=password + "\n", extra=env)
        a, b, g = [root / a for a in ("world A", "world B", "guest world")]
        for d in (a,b,g):
            d.mkdir()
        (a/"world.bin").write_bytes(os.urandom(9 * 1024 * 1024))
        (a/"regions").mkdir()
        (a/"regions"/"one.sav").write_bytes(b"first")
        (a/"empty").mkdir()
        for profile, folder in [(user_a,a), (user_b,b)]:
            cli(profile, "setup", "minecraft", "--title", "Minecraft", folder)
        assert f"saves: {b}" in cli(user_a, "--profile-dir", user_b, "status", "minecraft")
        assert f"saves: {a}" in cli(user_b, "status", "minecraft", "--profile-dir", user_a)
        cli(user_a, "sync", "minecraft")
        db = sqlite3.connect(root / "state.sqlite")
        def stats():
            return db.execute("SELECT count(*),sum(length(bytes)) FROM objects WHERE account='player'").fetchone()
        original = stats()
        cli(user_b, "sync", "minecraft")
        assert (b/"world.bin").read_bytes() == (a/"world.bin").read_bytes() and (b/"empty").is_dir()
        assert stats() == original
        cli(user_b, "run", "--game", "minecraft", "--", sys.executable, "-c",
            "import pathlib,sys; pathlib.Path(sys.argv[1]).write_text('game wrote this')", b/"regions"/"one.sav")
        cli(user_a, "sync", "minecraft")
        assert (a/"regions"/"one.sav").read_text() == "game wrote this"
        assert stats()[1] - original[1] < 1024
        print("PASS incremental two-device game session", flush=True)
        (a/"regions"/"one.sav").unlink()
        cli(user_a, "sync", "minecraft")
        cli(user_b, "sync", "minecraft")
        assert not (b/"regions"/"one.sav").exists()
        (a/"local.sav").write_text("desktop branch")
        (b/"on-deck.sav").write_text("deck branch")
        cli(user_a, "sync", "minecraft")
        e = cli(user_b, "sync", "minecraft", ok=False)
        assert "permanent backups" in e, e
        assert (b/"on-deck.sav").read_text() == "deck branch" and not (b/"local.sav").exists()
        assert db.execute("SELECT count(*) FROM versions WHERE pin='conflict'").fetchone()[0] == 2
        print("PASS permanent conflict backups before decision", flush=True)
        cli(user_a, "policy", "minecraft", "--shared", "true")
        assert "Minecraft" in cli(other, "catalog")
        cli(other, "setup", "minecraft", g)
        cli(other, "sync", "minecraft")
        assert not list(g.iterdir())
        for i in range(5):
            (a/"local.sav").write_text(f"version {i}")
            cli(user_a, "sync", "minecraft")
        rows = db.execute("SELECT id,pin FROM versions WHERE account='player' ORDER BY id DESC").fetchall()
        assert len([i for i,p in rows if not p]) == 4 and len([p for i,p in rows if p]) == 2
        cli(user_a, "pin", "minecraft", str(rows[1][0]))
        cli(user_a, "restore", "minecraft", str(rows[1][0]))
        assert (a/"local.sav").read_text() == "version 3"
        cli(user_a, "delete-backup", "minecraft", str(rows[1][0]))
        head = db.execute("SELECT head FROM games WHERE account='player'").fetchone()[0]
        cli(user_a,"backup","minecraft")
        cli(user_a,"unpin-current","minecraft",head)
        assert db.execute("SELECT pin FROM versions WHERE id=?",(head,)).fetchone() == ("",)
        assert db.execute("SELECT head FROM games WHERE account='player'").fetchone() == (head,)
        print("PASS retention, manual restore, deletion, private saves, shared recipe", flush=True)
        def login(name):
            return post(url, "/v1/login", {"account":"player","password":password,"device":name,"invite":None})
        x, y = login("one"), login("two")
        def rpc(tok, action, **fields):
            return post(url, "/v1/rpc", dict(action=action, **fields), tok)
        def acquire(tok, sid, takeover=None):
            return rpc(tok, "acquire", game="minecraft", session=sid, takeover=takeover)
        first = acquire(x, "one")["Granted"]
        busy = acquire(y, "two")["Busy"]
        second = acquire(y, "two", busy["generation"])["Granted"]
        try:
            rpc(x, "commit", lease=first["lease"], expected=first["head"]["id"], tree=first["head"]["tree"], close=True, op="old")
            raise AssertionError("superseded lease committed")
        except urllib.error.HTTPError as error:
            assert error.code == 412
        fields = dict(lease=second["lease"],expected=second["head"]["id"],tree=second["head"]["tree"],close=True,op="retrytest")
        assert rpc(y, "commit", **fields) == rpc(y, "commit", **fields)
        print("PASS fenced takeover and idempotent post-release retry", flush=True)
    finally:
        server.terminate()
        server.wait(timeout=10)
    stderr = server.stderr.read()
    ip = getattr(address, "ipv4_mapped", None) or address
    assert stderr.count("Warning: serving HTTP") == (0 if ip.is_loopback else 1), stderr
    print("PASS server config bootstrap, overwrite protection and HTTP startup warning", flush=True)


if __name__ == "__main__":
    with tempfile.TemporaryDirectory() as tmp:
        main(Path(tmp), ipaddress.ip_address(sys.argv[2] if len(sys.argv) > 2 else "127.0.0.1"))
