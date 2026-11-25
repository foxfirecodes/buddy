"""Exercise history and local exclusions against a disposable two-device server."""
import os
from pathlib import Path
import sqlite3
import subprocess
import sys
import tempfile

from sync import binary, cli, post


def run(root):
    config = root / "server.toml"
    config.write_text('listen="127.0.0.1:0"\ndatabase="server.sqlite"\n')
    server = subprocess.Popen([binary, "serve", "--config", config], stderr=subprocess.PIPE, text=True)
    try:
        line = server.stderr.readline().strip()
        assert line.startswith("Buddy listening on "), line
        url = "http://" + line.split()[-1]
        a, b = root / "client-a", root / "client-b"
        world_a, world_b = root / "world-a", root / "world-b"
        world_a.mkdir(); world_b.mkdir()
        env = {"BUDDY_ALLOW_FILE_CREDENTIAL": "true"}
        def call(profile, *args, **kwargs):
            return cli(profile, *args, extra=env, **kwargs)
        invite = call(a, "invite", "--config", config).strip()
        for profile, world in ((a, world_a), (b, world_b)):
            call(profile, "login", url, "player", "--device", profile.name, "--password-stdin",
                 *(["--invite", invite] if profile == a else []), input="password123\n")
            call(profile, "setup", "world", world)
        (world_a / "cache").mkdir()
        (world_a / "cache" / "local").write_text("original cache")
        (world_a / "cache2").mkdir()
        (world_a / "cache2" / "save").write_text("original save")
        call(a, "sync", "world")
        call(b, "sync", "world")
        busy_a, busy_b = root / "busy-a", root / "busy-b"
        busy_a.mkdir(); busy_b.mkdir()
        for profile, folder in ((a, busy_a), (b, busy_b)):
            call(profile, "setup", "busy", folder)
        call(b, "sync", "busy")
        token = post(url, "/v1/login", {"account": "player", "password": "password123",
                                        "device": "holder", "invite": None})
        granted = post(url, "/v1/rpc", {"action": "acquire", "game": "busy", "session": "holder",
                                        "takeover": None}, token)["Granted"]
        batch = subprocess.run([binary, "prepare-offline", "busy", "world"], text=True,
                               capture_output=True, timeout=50,
                               env=dict(os.environ, BUDDY_HOME=str(a), **env))
        assert batch.returncode and "busy" in batch.stderr, batch
        assert "world" in batch.stdout and "Ready" in batch.stdout, batch
        post(url, "/v1/rpc", {"action": "release", "lease": granted["lease"]}, token)
        print("PASS offline preparation continues after a busy game", flush=True)
        with sqlite3.connect(root / "server.sqlite") as db:
            original = db.execute("SELECT head FROM games WHERE game='world'").fetchone()[0]

        call(a, "ignore", "world", "saves/../outside", ok=False)
        call(a, "ignore", "world", "saves/cache")
        call(b, "sync", "world")
        call(a, "run", "--game", "world", "--", sys.executable, "-c",
             "import pathlib,sys; pathlib.Path(sys.argv[1]).write_text('local cache')",
             world_a / "cache" / "local")
        with sqlite3.connect(root / "server.sqlite") as db:
            assert db.execute("SELECT head FROM games WHERE game='world'").fetchone() == (original,)
        (world_b / "cache2" / "save").write_text("updated save")
        call(b, "sync", "world")
        call(a, "sync", "world")
        assert (world_a / "cache" / "local").read_text() == "local cache"
        assert (world_a / "cache2" / "save").read_text() == "updated save"
        call(a, "restore", "world", str(original))
        assert (world_a / "cache" / "local").read_text() == "local cache"
        assert (world_a / "cache2" / "save").read_text() == "original save"
        call(a, "ignore", "world", "saves/deep/protected")
        call(b, "sync", "world")
        (world_a / "deep").mkdir()
        (world_a / "deep" / "protected").write_text("local only")
        (world_b / "deep").mkdir()
        (world_b / "deep" / "synced").write_text("remote")
        call(b, "sync", "world")
        call(a, "sync", "world")
        assert (world_a / "deep" / "protected").read_text() == "local only"
        assert (world_a / "deep" / "synced").read_text() == "remote"
        (world_a / "deep" / "protected").write_text("edited locally")
        (world_b / "deep" / "synced").unlink()
        (world_b / "deep").rmdir()
        call(b, "sync", "world")
        call(a, "sync", "world")
        assert (world_a / "deep" / "protected").read_text() == "edited locally"
        assert not (world_a / "deep" / "synced").exists()
        call(a, "unignore", "world", "saves/cache")
        call(a, "sync", "world")
        with sqlite3.connect(root / "server.sqlite") as db:
            restored = db.execute("SELECT head FROM games WHERE game='world'").fetchone()[0]
        assert "saves/cache/local" in call(a, "changes", "world", str(restored))
        print("PASS exclusions preserve local files on sync and restore, respect path boundaries, and can be removed", flush=True)

        (world_a / "removed").write_text("old")
        call(a, "sync", "world")
        call(a, "policy", "world", "--retention", "0")
        with sqlite3.connect(root / "server.sqlite") as db:
            predecessor = db.execute("SELECT head FROM games WHERE game='world'").fetchone()[0]
        (world_a / "removed").unlink()
        (world_a / "added").write_text("new")
        (world_a / "cache2" / "save").write_text("modified")
        call(a, "sync", "world")
        with sqlite3.connect(root / "server.sqlite") as db:
            head = db.execute("SELECT head FROM games WHERE game='world'").fetchone()[0]
            assert db.execute("SELECT count(*) FROM versions WHERE id=?", (predecessor,)).fetchone()[0] == 0
        changes = call(a, "changes", "world", str(head))
        assert "Added\tsaves/added" in changes, changes
        assert "Modified\tsaves/cache2/save" in changes, changes
        assert "Removed\tsaves/removed" in changes, changes
        assert "unavailable" in call(a, "changes", "busy", str(head), ok=False)
        print("PASS historical changes survive retention of the predecessor", flush=True)

        # Simulate snapshots saved before Buddy recorded changes at commit time.
        with sqlite3.connect(root / "server.sqlite") as db:
            db.execute("DELETE FROM backup_changes WHERE version=?", (head,))
        assert "previous version unavailable" in call(a, "changes", "world", str(head))
        call(a, "policy", "world", "--retention", "3")
        (world_a / "added").write_text("later")
        call(a, "sync", "world")
        with sqlite3.connect(root / "server.sqlite") as db:
            latest = db.execute("SELECT head FROM games WHERE game='world'").fetchone()[0]
            db.execute("DELETE FROM backup_changes WHERE version=?", (latest,))
        assert f"earlier retained backup #{head}" in call(a, "changes", "world", str(latest))
        print("PASS legacy backups identify the available comparison", flush=True)
    finally:
        server.terminate()
        server.wait(timeout=10)


if __name__ == "__main__":
    with tempfile.TemporaryDirectory() as tmp:
        run(Path(tmp))
