"""Test local Steam discovery, imported suggestions and disconnected recipe editing."""
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import tempfile

from sync import binary, cli


def run(root):
    cfg = root / "server.toml"
    cfg.write_text('listen="127.0.0.1:0"\ndatabase="server.sqlite"\n')
    def start():
        server = subprocess.Popen([binary, "server", "serve", "--config", cfg], stderr=subprocess.PIPE, text=True)
        return server, server.stderr.readline().split()[-1]
    server, addr = start()
    a, b = root / "a", root / "b"
    cfg.write_text(f'listen="{addr}"\ndatabase="server.sqlite"\n')
    try:
        steam, other = root / "Steam", root / "secondary drive"
        (steam / "steamapps").mkdir(parents=True)
        install = other / "steamapps/common/Test Game"
        install.mkdir(parents=True)
        prefix = other / "steamapps/compatdata/42/pfx"
        prefix.mkdir(parents=True)
        (steam / "steamapps/libraryfolders.vdf").write_text('"libraryfolders" { '+ ' '.join(
            f'"{i}" {{ "path" "{p}" }}' for i,p in enumerate([steam, root / "unplugged", other])) + ' }')
        (other / "steamapps/appmanifest_42.acf").write_text(
            '"AppState" { "appid" "42" "name" "Test Library Title" "installdir" "Test Game" }')
        (install / "progress.bin").write_bytes(b"fixture save")
        env = {"BUDDY_STEAM_ROOT": str(steam), "BUDDY_ALLOW_FILE_CREDENTIAL":"true"}
        def call(home, *args, **kwargs):
            return cli(home, *args, extra=env, **kwargs)
        invite = call(a, "server", "invite", "--config", cfg).strip()
        for profile in (a,b):
            call(profile,"login","http://"+addr,"player","--password-stdin","--device",profile.name,
                 *(["--invite",invite] if profile == a else []),input="test-password\n")
        assert "Test Library Title" in call(a,"games")
        catalog = root / "example.yaml"
        catalog.write_text('Test Game:\n  steam: {id: 42}\n  files:\n    "<base>/progress.bin": {tags: [save], when: [{os: windows}]}\n')
        call(a,"load-manifest",catalog)
        assert str(install / "progress.bin") in call(a,"suggestions","steam-42")
        call(a,"setup","steam-42","--file",install / "progress.bin")
        with sqlite3.connect(root / "server.sqlite") as db:
            recipe = json.loads(db.execute("SELECT recipe FROM games").fetchone()[0])
            assert recipe["slots"]["saves"]["locations"]["proton"] == {"root":"install","relative":"progress.bin"}
        call(a,"sync","steam-42")
        call(b,"setup","steam-42","--file",root / "guest.bin")
        call(b,"sync","steam-42")
        print("PASS installed title, Proton recipe and approved manifest suggestion on a second library",flush=True)

        server.terminate();server.wait(timeout=6)
        call(a,"policy","steam-42","--retention","7")
        custom = root / "instance"
        custom.mkdir()
        settings = custom / "options.json"
        settings.write_text('{"sound":true}')
        call(a,"root","steam-42","instance",custom)
        call(a,"setup","steam-42","--settings","--slot","options","--file",settings)
        assert "Recipe edits queued" in call(a,"status","steam-42")
        server,_ = start()
        call(b,"policy","steam-42","--shared","true")
        call(a,"sync","steam-42")
        with sqlite3.connect(root / "server.sqlite") as db:
            value, shared = db.execute("SELECT recipe,shared FROM games").fetchone()
            recipe = json.loads(value)
            assert recipe["retention"] == 7 and shared == 1
            assert recipe["slots"]["options"]["locations"]["proton"] == {"root":"instance","relative":"options.json"}
        call(b,"sync","steam-42")
        server.terminate();server.wait(timeout=6)
        call(a,"policy","steam-42","--retention","1")
        server,_ = start()
        call(b,"policy","steam-42","--retention","8")
        assert "Choose a recipe setting" in call(a,"sync","steam-42",ok=False)
        assert "Recipe edits queued" in call(a,"status","steam-42")
        call(a,"discard-setup","steam-42")
        call(a,"sync","steam-42")
        print("PASS offline edits survive restart, merge disjoint fields and pause conflicting edits",flush=True)
    finally:
        if server.poll() is None:
            server.terminate();server.wait(timeout=8)


with tempfile.TemporaryDirectory() as name:
    run(Path(name))
