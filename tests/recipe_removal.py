"""Exercise account-wide recipe removal against a live server and three device profiles."""
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import sys
import tempfile
import time
from urllib.error import HTTPError

from sync import binary, cli, post


def run(root):
    cfg = root / "server.toml"
    cfg.write_text('listen="127.0.0.1:0"\ndatabase="server.sqlite"\n')

    def start():
        process = subprocess.Popen([binary, "server", "serve", "--config", cfg], stderr=subprocess.PIPE, text=True)
        line = process.stderr.readline()
        assert line.startswith("Buddy listening on "), line
        return process, line.split()[-1]

    server, addr = start()
    url = "http://" + addr
    cfg.write_text(f'listen="{addr}"\ndatabase="server.sqlite"\n')
    a, b, c = (root / f"profile-{name}" for name in "ABC")
    slots = {}
    game = "sample"
    waiting = None
    try:
        invite = cli(a, "server", "invite", "--config", cfg).strip()
        for profile in [a, b, c]:
            args = ["--invite", invite] if profile == a else []
            cli(profile, "login", url, "player", "--password-stdin", "--private-file-credential",
                "--device", profile.name, *args, input="test-password\n")
            folder = root / f"world-{profile.name}"
            folder.mkdir()
            settings = root / f"{profile.name}.ini"
            slots[profile] = (folder, settings)
            cli(profile, "setup", game, folder)
            cli(profile, "setup", game, "--file", "--slot", "settings", settings)
        world, settings = slots[a]
        (world / "progress").write_bytes(b"before")
        settings.write_bytes(b"old settings")
        cli(a, "backup", game)
        for profile in [b, c]:
            cli(profile, "sync", game)

        token = post(url, "/v1/login", dict(account="player", password="test-password", device="review", invite=None))
        def rpc(action, **kwargs):
            return post(url, "/v1/rpc", dict(action=action, **kwargs), token)
        def expect_error(code, action, **kwargs):
            try:
                rpc(action, **kwargs)
            except HTTPError as error:
                assert error.code == code, error.read()
            else:
                raise AssertionError(f"{action} unexpectedly succeeded")
        original = rpc("games")[0]
        backup = rpc("backups", game=game)[0]["id"]
        lease = rpc("acquire", game=game, session="playing", takeover=None)["Granted"]["lease"]
        expect_error(412, "remove_slot", game=game, slot="saves", expected=original["revision"])
        rpc("release", lease=lease)
        # The game loses its connection after launch. Its normal exit captures immutable
        # pending bytes for both slots. Move the retired folder off-drive before reconnect.
        ready, finish = root / "ready", root / "finish"
        script = """from pathlib import Path
import sys, time
ready, finish, world, settings = map(Path, sys.argv[1:])
ready.touch()
while not finish.exists():
    time.sleep(.03)
(world / 'progress').write_bytes(b'offline world')
settings.write_bytes(b'offline settings')
"""
        waiting = subprocess.Popen(
            [binary, "run", "--game", game, "--", sys.executable, "-c", script, ready, finish, *slots[b]],
            env={**os.environ, "BUDDY_HOME": str(b), "DISPLAY": "", "WAYLAND_DISPLAY": ""},
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        deadline = time.monotonic() + 20
        while not ready.exists() and time.monotonic() < deadline and waiting.poll() is None:
            time.sleep(.05)
        assert ready.exists()
        server.terminate()
        server.wait(timeout=6)
        finish.touch()
        assert waiting.wait(timeout=25) == 0
        assert "1 queued" in cli(b, "status", game)
        detached = root / "unplugged"
        slots[b][0].rename(detached)
        server, _ = start()
        held = rpc("acquire", game=game, session="recovery", takeover=None)["Busy"]
        takeover = rpc("acquire", game=game, session="recovery", takeover=held["generation"])["Granted"]
        rpc("release", lease=takeover["lease"])

        cli(a, "remove-slot", game, "saves")
        current = rpc("games")[0]
        assert list(current["recipe"]["slots"]) == ["settings"] and current["retired_slots"] == ["saves"]
        assert (world / "progress").read_bytes() == b"before"
        expect_error(412, "remove_slot", game=game, slot="settings", expected=original["revision"])
        expect_error(400, "save_recipe", recipe=original["recipe"], expected=current["revision"], shared=False)
        old = rpc("read_revision", game=game, id=backup)
        assert old["tree"]["saves/progress"]["File"] and old["tree"]["settings"]["File"]
        cli(b, "sync", game)
        assert "0 queued" in cli(b, "status", game) and "saves:" not in cli(b, "status", game)
        assert (detached / "progress").read_bytes() == b"offline world"
        versions = rpc("backups", game=game)
        archived = [v["id"] for v in versions if v["reason"] == "offline"]
        assert len(archived) == 1
        tree = rpc("read_revision", game=game, id=archived[0])["tree"]
        with sqlite3.connect(root / "server.sqlite") as db:
            def data(name):
                return b"".join(db.execute("SELECT bytes FROM objects WHERE account='player' AND hash=?",
                                           [chunk["hash"]]).fetchone()[0] for chunk in tree[name]["File"]["chunks"])
            assert data("saves/progress") == b"offline world" and data("settings") == b"offline settings"
        cli(c, "sync", game)
        assert slots[c][1].read_bytes() == b"offline settings"
        (slots[c][0] / "progress").write_bytes(b"local only")
        cli(c, "restore", game, str(backup))
        assert slots[c][1].read_bytes() == b"old settings"
        assert (slots[c][0] / "progress").read_bytes() == b"local only"
        cli(a, "remove-slot", game, "settings")
        cli(c, "sync", game)
        assert not rpc("games")[0]["recipe"]["slots"]
        head = next(v for v in rpc("backups", game=game) if v["head"])
        assert rpc("read_revision", game=game, id=head["id"])["tree"] == {}
        assert slots[c][1].read_bytes() == b"old settings"
        assert "settings:" not in cli(c, "status", game)
        assert "choose a new --slot" in cli(a, "setup", game, world, ok=False)
        cli(a, "setup", game, "--slot", "world-2", world)
        cli(a, "sync", game)
        assert list(rpc("games")[0]["recipe"]["slots"]) == ["world-2"]
        print("PASS three-device removal, detached offline progress, CAS, historical restore, final slot and re-enrollment")
    finally:
        if waiting and waiting.poll() is None:
            (root / "finish").touch()
            waiting.kill()
            waiting.wait()
        if server.poll() is None:
            server.terminate()
            server.wait(timeout=8)


with tempfile.TemporaryDirectory() as name:
    run(Path(name))
