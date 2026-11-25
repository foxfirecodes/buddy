"""Exercise a copied install command and signed updates against disposable Linux hosts."""
import hashlib
import json
import os
from pathlib import Path
import shutil
import sqlite3
import subprocess
import sys
import tempfile
import urllib.request
from concurrent.futures import ThreadPoolExecutor
from urllib.error import HTTPError

BUDDY = Path(sys.argv[1]).resolve(strict=True)
PLATFORM = "linux-x86_64"


def execute(args, *, env=None, ok=True, input=None):
    result = subprocess.run(args, input=input, text=True, capture_output=True, env=env, timeout=90)
    assert (result.returncode == 0) == ok, f"command status {result.returncode}: {result.stderr[-400:]}"
    return result.stdout if ok else result.stderr


def issue(url, token):
    req = urllib.request.Request(url + "/v1/device-links", method="POST",
                                 headers={"Authorization": "Bearer " + token})
    return json.load(urllib.request.urlopen(req, timeout=10))


def run(root, tls):
    root.mkdir()
    db = root / "server.sqlite"
    cfg = root / "server.toml"
    cfg.write_text('listen="127.0.0.1:0"\ndatabase="server.sqlite"\n')
    if tls:
        openssl = ["openssl", "req", "-newkey", "rsa:2048", "-nodes"]
        execute(openssl + ["-x509", "-keyout", str(root/"ca.key"), "-out", str(root/"ca.pem"),
                           "-days", "2", "-subj", "/CN=Fixture CA", "-addext", "basicConstraints=critical,CA:TRUE"])
        execute(openssl + ["-keyout", str(root/"server.key"), "-out", str(root/"server.csr"),
                           "-subj", "/CN=loopback"])
        (root/"extensions").write_text("subjectAltName=IP:127.0.0.1\nextendedKeyUsage=serverAuth\n")
        execute(["openssl", "x509", "-req", "-in", str(root/"server.csr"), "-CA", str(root/"ca.pem"),
                 "-CAkey", str(root/"ca.key"), "-CAcreateserial", "-days", "2", "-extfile",
                 str(root/"extensions"), "-out", str(root/"server.pem")])
        with cfg.open("a") as f:
            f.write('[tls]\ncertificate="server.pem"\nprivate_key="server.key"\n')
    server = subprocess.Popen([BUDDY, "serve", "--config", cfg], stderr=subprocess.PIPE, text=True)
    owner = root / "desktop"
    second = root / "receiver"
    bindir = root / "bin with space"
    env = dict(os.environ, BUDDY_HOME=str(second), XDG_BIN_HOME=str(bindir),
               BUDDY_STEAM_ROOT=str(root/"steam"), BUDDY_ALLOW_FILE_CREDENTIAL="true")
    env.pop("DISPLAY", None)
    env.pop("WAYLAND_DISPLAY", None)
    try:
        line = server.stderr.readline()
        assert "Buddy listening" in line, line
        url = ("https://" if tls else "http://") + line.strip().split()[-1]
        owner_cli = [BUDDY, "--profile-dir", owner]
        invite = execute([BUDDY, "invite", "--config", cfg]).strip()
        pairing = []
        if tls:
            pair = root/"pair.json"
            pair.write_text(json.dumps(dict(version=1, ca=(root/"ca.pem").read_text(), url=url)))
            pairing = ["--pairing", pair]
        execute(owner_cli + ["login", url, "player", "--device", "desktop", "--invite", invite,
                             "--password-stdin", "--private-file-credential"] + pairing, input="a-fixture-password\n")
        publish = [BUDDY, "publish-release", "--config", cfg]
        link = owner_cli + ["install-command", PLATFORM]
        assert "publish-release" in execute(link, ok=False)
        assert "does not match" in execute(publish + ["windows-x86_64", BUDDY], ok=False)
        report = execute(publish + [PLATFORM, BUDDY])
        assert "release 1\n" in report
        pin = report.strip().split()[-1]
        assert report == execute(publish + [PLATFORM, BUDDY])
        if not tls:
            assert "--trust-key" in execute(link, ok=False)
            assert "differs" in execute(link + ["--trust-key", "0" * 64], ok=False)
        command = execute(link + ([] if tls else ["--trust-key", pin])).strip()
        # Simulate damaged metadata or files in storage. Neither may execute or consume the link.
        dbcon = sqlite3.connect(db)
        with dbcon:
            manifest, script = dbcon.execute("SELECT manifest,installer FROM releases").fetchone()
            code_hash = dbcon.execute("SELECT hash FROM device_links").fetchone()[0]
            altered = json.loads(manifest)
            altered["release"]["generation"] += 1
            dbcon.execute("UPDATE releases SET manifest=?", [json.dumps(altered)])
        assert "signature verification failed" in execute(link, ok=False)
        with dbcon:
            dbcon.execute("UPDATE releases SET manifest=?", [manifest])
            dbcon.execute("UPDATE releases SET installer='exit 0'")
        execute(["sh", "-c", command], env=env, ok=False)
        assert not (bindir/"buddy").exists()
        with dbcon:
            dbcon.execute("UPDATE releases SET installer=?", [script])
        digest = json.loads(manifest)["release"]["sha256"]
        image = Path(str(db) + ".releases") / digest
        with image.open("r+b") as f:
            f.seek(-1, 2)
            previous = f.read(1)
            f.seek(-1, 2)
            f.write(bytes([previous[0] ^ 255]))
        assert "verification" in execute(["sh", "-c", command], env=env, ok=False)
        with image.open("r+b") as f:
            f.seek(-1, 2)
            f.write(previous)
        assert dbcon.execute("SELECT count(*) FROM device_links").fetchone()[0] == 1
        assert not (bindir/"buddy").exists()
        # An unrelated pre-existing binary must survive an assisted install.
        installed = bindir/"buddy"
        installed.write_bytes(b"keep me")
        assert "Already installed" in execute(["sh", "-c", command], env=env, ok=False)
        assert installed.read_bytes() == b"keep me"
        installed.unlink()
        execute(["sh", "-c", command], env=env)
        assert hashlib.sha256(installed.read_bytes()).hexdigest() == digest
        execute([installed, "games"], env=env)
        assert dbcon.execute("SELECT count(*) FROM device_links").fetchone()[0] == 0
        # Simulate losing the response after the server consumed the code and before
        # the local transaction activated the already staged credential.
        with sqlite3.connect(second/"client.sqlite3") as profile:
            active = json.loads(profile.execute("SELECT v FROM state WHERE k='login'").fetchone()[0])
            profile.execute("DELETE FROM state WHERE k='login'")
            profile.execute("INSERT INTO state VALUES('pending-enrollment',?)",
                            [json.dumps(dict(code_hash=code_hash, record=active))])
        execute(["sh", "-c", command], env=env)
        assert dbcon.execute("SELECT count(*) FROM devices").fetchone()[0] == 2
        second_try = dict(env, BUDDY_HOME=str(root/"third-device"))
        assert "used or expired" in execute(["sh", "-c", command], env=second_try, ok=False)
        fresh_bin = root / "third-bin"
        second_try["XDG_BIN_HOME"] = str(fresh_bin)
        assert "used or expired" in execute(["sh", "-c", command], env=second_try, ok=False)
        assert list(fresh_bin.iterdir()) == [], "failed enrollment left an executable behind"
        generation_two = root/"new-build"
        shutil.copyfile(BUDDY, generation_two)
        with generation_two.open("ab") as f:
            f.write(b"fixture build 2")
        assert "release 2" in execute(publish + [PLATFORM, generation_two])
        retry_command = execute(link).strip()
        other = root / "other-account"
        invite = execute([BUDDY, "invite", "--config", cfg]).strip()
        execute([BUDDY, "--profile-dir", other, "login", url, "other", "--device", "desktop",
                 "--invite", invite, "--password-stdin", "--private-file-credential"] + pairing,
                input="another-fixture-password\n")
        other_env = dict(second_try, BUDDY_HOME=str(other))
        assert "different account or server" in execute(["sh", "-c", retry_command], env=other_env, ok=False)
        assert list(fresh_bin.iterdir()) == []
        with sqlite3.connect(other/"client.sqlite3") as profile:
            assert json.loads(profile.execute("SELECT v FROM state WHERE k='login'").fetchone()[0])["account"] == "other"
        execute(["sh", "-c", retry_command], env=second_try)
        assert (fresh_bin/"buddy").read_bytes() == generation_two.read_bytes()
        execute([fresh_bin/"buddy", "games"], env=second_try)
        path_home = root/"path-home"
        (path_home/"bin").mkdir(parents=True)
        path_env = dict(second_try, HOME=str(path_home), PATH=str(path_home/"bin") + ":" + os.environ["PATH"])
        del path_env["XDG_BIN_HOME"]
        assert "export PATH" not in execute(["sh", "-c", retry_command], env=path_env)
        assert (path_home/"bin"/"buddy").exists()
        path_env["PATH"] = os.environ["PATH"]
        path_env["SHELL"] = "/bin/sh"
        assert 'export PATH="$HOME/.local/bin:$PATH"' in execute(["sh", "-c", retry_command], env=path_env)
        assert (path_home/".local/bin"/"buddy").exists()
        execute([installed, "update"], env=env)
        assert hashlib.sha256(installed.read_bytes()).digest() == hashlib.sha256(generation_two.read_bytes()).digest()
        assert "up to date" in execute([installed, "update"], env=env).lower()
        with dbcon:
            # Simulate an older signed reply being replayed on a client that saw generation two.
            dbcon.execute("UPDATE releases SET manifest=? WHERE generation=2", [manifest])
        assert "older" in execute([installed, "update"], env=env, ok=False)
        key_path = image.parent/"signing-key.pk8"
        original_key = key_path.with_suffix(".original")
        key_path.rename(original_key)
        with dbcon:
            # Simulate metadata from a previously published platform. The server key
            # belongs to the host even when this platform has never been published.
            dbcon.execute("UPDATE releases SET platform='windows-x86_64'")
        assert "restore the original release signing key" in execute(publish + [PLATFORM, generation_two], ok=False)
        original_key.rename(key_path)
        if not tls:
            with sqlite3.connect(owner/"client.sqlite3") as profile:
                cred = json.loads(profile.execute("SELECT v FROM state WHERE k='login'").fetchone()[0])
            token = cred["token"]
            def redeem(code):
                import secrets
                request = urllib.request.Request(url + "/v1/device-links/redeem",
                    data=json.dumps(dict(code=code, device="test", token_hash=secrets.token_hex(32))).encode(),
                    headers={"Content-Type":"application/json"})
                try:
                    return urllib.request.urlopen(request, timeout=10).status
                except HTTPError as error:
                    return error.code
            expired = issue(url, token)
            with dbcon:
                dbcon.execute("UPDATE device_links SET expires=0")
            assert redeem(expired) == 401
            try:
                issue(url, "no such bearer")
            except HTTPError as e:
                assert e.code == 401
            else:
                raise AssertionError("unauthorized device code")
            one_use = issue(url, token)
            with ThreadPoolExecutor(2) as pool:
                assert sorted(pool.map(redeem, [one_use, one_use])) == [204, 401]
        dbcon.close()
    finally:
        server.terminate()
        server.wait(timeout=5)
    print("PASS", "paired HTTPS" if tls else "HTTP", "install, integrity, enrollment, update, replay")


if __name__ == "__main__":
    with tempfile.TemporaryDirectory() as name:
        for tls in (False, True):
            run(Path(name)/str(tls), tls)
