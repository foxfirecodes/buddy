"""Exercise interrupted restores and independently bound slots against the real binary."""
import os
from pathlib import Path
import sqlite3
import subprocess
import sys
import tempfile
import time

from sync import binary, cli


def interrupted_restore(profile, world, old, new):
    p = subprocess.Popen([binary, "sync", "world"], env=dict(os.environ, BUDDY_HOME=str(profile)))
    try:
        deadline = time.monotonic() + 45
        while time.monotonic() < deadline and p.poll() is None:
            if (world / "0000").read_text() == new and (world / "0999").read_text() == old:
                p.kill()
                p.wait(timeout=5)
                assert "restore pending=true" in cli(profile, "status", "world")
                return
            time.sleep(0.001)
        raise AssertionError("did not intercept a partially installed restore")
    finally:
        if p.poll() is None:
            p.kill()
            p.wait()


def run(root):
    cfg = root / "server.toml"
    cfg.write_text('listen="127.0.0.1:0"\ndatabase="server.sqlite"\n')
    server = subprocess.Popen([binary, "server", "serve", "--config", cfg], stderr=subprocess.PIPE, text=True)
    try:
        url = "http://" + server.stderr.readline().split()[-1]
        pa, pb = root / "profile-a", root / "profile-b"
        a, b = root / "world-a", root / "world-b"
        for profile, world in [(pa, a), (pb, b)]:
            world.mkdir()
            flags = ["--invite", cli(profile, "server", "invite", "--config", cfg).strip()] if profile == pa else []
            cli(profile, "login", url, "player", "--password-stdin", "--private-file-credential",
                *flags, input="test-password\n")
            cli(profile, "setup", "world", world)
        for i in range(1000):
            (a / f"{i:04}").write_text("first")
        cli(pa, "sync", "world")
        cli(pb, "sync", "world")
        for p in a.iterdir():
            p.write_text("second")
        cli(pa, "sync", "world")
        interrupted_restore(pb, b, "first", "second")
        cli(pb, "rollback", "world")
        assert len(list(b.iterdir())) == 1000 and all(p.read_text() == "first" for p in b.iterdir())
        interrupted_restore(pb, b, "first", "second")
        cli(pb, "run", "--game", "world", "--", sys.executable, "-c",
            "from pathlib import Path; import sys; assert all(p.read_text()=='second' for p in Path(sys.argv[1]).iterdir())", b)
        print("PASS rollback and roll-forward after process death during a live restore", flush=True)

        settings = root / "settings.ini"
        settings.write_text("volume=9")
        cli(pa, "setup", "world", "--slot", "settings", "--file", "--settings", settings)
        cli(pa, "sync", "world")
        (b / "0000").write_text("edited on B")
        cli(pb, "sync", "world")
        cli(pa, "sync", "world")
        assert settings.read_text() == "volume=9" and (a / "0000").read_text() == "edited on B"
        second = root / "device-b.ini"
        cli(pb, "setup", "world", "--slot", "settings", "--file", second)
        cli(pb, "sync", "world")
        assert second.read_text() == "volume=9"
        cli(pb, "unbind", "world", "settings")
        second.write_text("device only")
        (b / "0001").write_text("after unbind")
        cli(pb, "sync", "world")
        cli(pa, "sync", "world")
        assert settings.read_text() == "volume=9" and second.read_text() == "device only"
        print("PASS unbound slots keep their remote and local contents", flush=True)

        large = a / "large.sav"
        large.write_bytes(b"x" * (2 * 1024 * 1024))
        time.sleep(2.2)
        cli(pa, "sync", "world")
        cli(pb, "sync", "world")
        original = large.stat()
        with large.open("r+b") as f:
            f.seek(1024 * 1024)
            f.write(b"y")
        os.utime(large, ns=(original.st_atime_ns, original.st_mtime_ns))
        cli(pa, "sync", "world")
        cli(pb, "sync", "world")
        assert (b / "large.sav").read_bytes() == large.read_bytes()
        print("PASS same-length changes with preserved mtime invalidate the large-file cache", flush=True)

        with sqlite3.connect(root / "server.sqlite") as db:
            head = db.execute("SELECT head FROM games").fetchone()
            b.rename(root / "other-drive")
            b.mkdir()
            assert "drive or root changed" in cli(pb, "sync", "world", ok=False)
            assert db.execute("SELECT head FROM games").fetchone() == head
        print("PASS a replacement mount cannot publish an empty world", flush=True)
    finally:
        server.terminate()
        server.wait(timeout=10)


with tempfile.TemporaryDirectory() as tmp:
    run(Path(tmp))
