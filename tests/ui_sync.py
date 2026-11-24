"""Run under xvfb-run on an isolated display. Exercise decisions with the shipping Linux GUI."""
import json
import os
import re
import socket
import sqlite3
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import urllib.request

binary = str(Path(sys.argv[1]).resolve(strict=True))
root = Path(sys.argv[2]).resolve(strict=True)
assert os.environ.get("BUDDY_TEST_X11") == "isolated", "start under an isolated Xvfb as documented"
env = dict(os.environ, BUDDY_ALLOW_FILE_CREDENTIAL="true")


def args(profile):
    return dict(env, BUDDY_HOME=str(root / profile))


def call(profile, *argv, data=None):
    return subprocess.run([binary,*map(str,argv)], env=args(profile), input=data, capture_output=True,
                          text=True, timeout=45, check=True).stdout


def window(title, action):
    wid = subprocess.check_output(["timeout","25","xdotool","search","--sync","--onlyvisible","--name",
                                   "^"+re.escape(title)+"$"], text=True).splitlines()[-1]
    subprocess.run(["xdotool","windowactivate","--sync",wid],timeout=5,check=True)
    time.sleep(0.3)
    subprocess.run(["import","-window",wid,str(root/(title+".png"))], check=True, timeout=6)
    if action:
        subprocess.run(["xdotool","key","--clearmodifiers","--delay","130",*action],check=True)
    return wid


def rpc(url, token, action, **fields):
    data=json.dumps(dict(action=action, **fields)).encode()
    request=urllib.request.Request(url+"/v1/rpc",data=data,headers={"Content-Type":"application/json","Authorization":"Bearer "+token})
    with urllib.request.urlopen(request,timeout=8) as response:
        return json.load(response)


def start(config):
    server=subprocess.Popen([binary,"serve","--config",str(config)], stderr=subprocess.PIPE,text=True)
    line=server.stderr.readline().strip()
    assert line.startswith("Buddy listening on "),line
    return server,line.split()[-1]


def progress_with_stalled_display():
    # Accept the X11 connection but never finish its handshake. No frames can render.
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        listener.listen()
        listener.settimeout(5)
        port = listener.getsockname()[1]
        stalled = dict(env, DISPLAY=f"127.0.0.1:{port-6000}", XDG_SESSION_TYPE="x11")
        stalled.pop("WAYLAND_DISPLAY", None)
        stalled.pop("WAYLAND_SOCKET", None)
        proc = subprocess.Popen([binary, "_progress"], stdin=subprocess.PIPE, env=stalled)
        try:
            conn, _ = listener.accept()
            with conn:
                assert proc.poll() is None
                proc.stdin.close()
                assert proc.wait(timeout=3) == 0
        finally:
            if proc.poll() is None:
                proc.kill()
                proc.wait()
    print("PASS progress exits on wrapper EOF with an unresponsive display", flush=True)


def first_proton_play():
    app = "steam-424242"
    seed_compat = root/"populated source"
    rel = Path("drive_c/users/steamuser/AppData/Roaming/Fixture Game")
    seed = seed_compat/"pfx"/rel
    seed.mkdir(parents=True)
    (seed/"game.sav").write_bytes(b"pre-existing server save")
    def steam(profile, data):
        return dict(args(profile), SteamAppId="424242", STEAM_COMPAT_DATA_PATH=str(data),
                    STEAM_COMPAT_TOOL_PATHS=str(root/"compatibility tool"))
    def seed_call(*argv):
        subprocess.run([binary, *map(str,argv)], env=steam("A",seed_compat), check=True)
    seed_call("setup", app, seed)
    seed_call("sync", app)

    compat = root/"new prefix"
    tool = root/"compatibility tool"/"proton"
    tool.parent.mkdir()
    tool.write_text("""#!/usr/bin/env python3
import os, sys
from pathlib import Path
p = Path(os.environ["STEAM_COMPAT_DATA_PATH"])/"pfx"
if sys.argv[1:] == ["getcompatpath", "/"]:
    assert "BUDDY_INVITE" not in os.environ and "BUDDY_PASSWORD" not in os.environ
    (p/"drive_c/users/steamuser/AppData/Roaming").mkdir(parents=True)
    (p/"user.reg").touch()
elif sys.argv[1:] == ["waitforexitandrun", "fixture game.exe", "a b"]:
    assert (p/"drive_c/users/steamuser/AppData/Roaming/Fixture Game/game.sav").read_bytes() == b"pre-existing server save"
    (p/"first-game-entry").touch()
else:
    raise RuntimeError("unexpected launch arguments")
""")
    tool.chmod(0o755)
    runtime = root/"_v2-entry-point"
    runtime.write_text('#!/bin/sh\ncase "$1" in --verb=getcompatpath|--verb=waitforexitandrun) shift;; *) exit 5;; esac\n[ "$1" = -- ] || exit 7\nshift\nexec "$@"\n')
    runtime.chmod(0o755)
    command = [binary, "run", "--game", app, "--", runtime, "--verb=waitforexitandrun", "--",
               tool, "waitforexitandrun", "fixture game.exe", "a b"]
    def play():
        return subprocess.Popen(command, env=dict(steam("B",compat),BUDDY_PASSWORD="test",BUDDY_INVITE="test"))
    proc = play()
    window("Set up the Proton prefix", ["Return"])
    window("Buddy needs attention", ["Return"])
    assert proc.wait(timeout=10) != 0 and not compat.exists()
    proc = play()
    window("Set up the Proton prefix", ["Right","Return"])
    window("Confirm Proton save locations", ["Return"])
    window("Buddy needs attention", ["Return"])
    assert proc.wait(timeout=10) != 0 and not (compat/"pfx"/rel).exists()
    proc = play()
    window("Confirm Proton save locations", ["Right","Return"])
    assert proc.wait(timeout=15) == 0 and (compat/"pfx/first-game-entry").is_file()
    call("B", "unbind", app, "saves")
    (compat/"pfx/first-game-entry").unlink()
    proc = play()
    window("Buddy needs attention", ["Return"])
    assert proc.wait(timeout=10) != 0 and not (compat/"pfx/first-game-entry").exists()
    assert "saves:" not in call("B", "status", app)
    print("PASS first Proton enrollment respects cancellation, restores before launch and preserves explicit unbind", flush=True)


def run():
    progress_with_stalled_display()
    config=root/"server.toml"
    config.write_text('listen="127.0.0.1:0"\ndatabase="server.sqlite"\n')
    server,addr=start(config);url="http://"+addr
    config.write_text(f'listen="{addr}"\ndatabase="server.sqlite"\n')
    a,b=root/"world-A",root/"world-B"
    a.mkdir();b.mkdir()
    try:
        gui=subprocess.Popen([binary,"gui"],env=args("A"))
        try:
            window("Buddy",None)
        finally:
            gui.terminate();gui.wait(timeout=4)
        invite=call("A","invite","--config",config).strip()
        for profile,folder in [("A",a),("B",b)]:
            flags=["--invite",invite] if profile=="A" else []
            call(profile,"login",url,"player","--device",profile,"--password-stdin",*flags,data="password123\n")
            call(profile,"setup","world",folder)
        (a/"save").write_text("baseline")
        call("A","sync","world");call("B","sync","world")
        server.terminate();server.wait(timeout=8)
        def play(profile,folder,text):
            return subprocess.Popen([binary,"run","--game","world","--",sys.executable,"-c",
                "from pathlib import Path; import sys; Path(sys.argv[1]).write_text(sys.argv[2])",str(folder/"save"),text],env=args(profile))
        offline=play("A",a,"offline A")
        window("Server unreachable",["Left","Return"])
        assert offline.wait(timeout=15)==0
        assert "1 queued" in call("A","status","world")
        server,_=start(config)
        (b/"save").write_text("online B");call("B","sync","world")
        def server_head():
            with sqlite3.connect(root / "server.sqlite") as db:
                return db.execute("SELECT head FROM games WHERE game='world'").fetchone()[0]
        before=server_head()
        resolving=play("A",a,"after choosing server")
        conflict_title="Which save should Buddy continue with?"
        window(conflict_title,["Left"])
        # Highlighting a version only selects it; publication requires confirmation.
        window(conflict_title,None)
        assert resolving.poll() is None and server_head()==before
        assert (a/"save").read_text()=="offline A"
        window(conflict_title,["Return"])
        assert resolving.wait(timeout=20)==0
        assert "0 queued" in call("A","status","world")
        history=call("A","backups","world")
        assert history.count("conflict")==2,history
        print("PASS graphical offline session, pinned conflict and reconciliation",flush=True)
        login=urllib.request.Request(url+"/v1/login",data=json.dumps({"account":"player","password":"password123","device":"sleeping PC","invite":None}).encode(),headers={"Content-Type":"application/json"})
        with urllib.request.urlopen(login) as r: token=json.load(r)
        holder=rpc(url,token,"acquire",game="world",session="sleeping",takeover=None)["Granted"]["lease"]
        contender=play("A",a,"after takeover")
        window("Another device has this game open",["Left","Return"])
        # At the second screen the prior holder must still own the lease.
        window("Confirm takeover",None)
        assert rpc(url,token,"renew",lease=holder) is None
        subprocess.run(["xdotool","key","--clearmodifiers","--delay","120","Right","Return"],check=True)
        assert contender.wait(timeout=15)==0
        call("B","sync","world")
        assert (b/"save").read_text()=="after takeover"
        print("PASS two distinct graphical takeover steps and subsequent restore",flush=True)
        gate, ready = root / "finish-game", root / "game-started"
        script = ("import pathlib,sys,time; save,ready,gate=map(pathlib.Path,sys.argv[1:]); "
                  "ready.touch();\nwhile not gate.exists(): time.sleep(0.03)\n"
                  "save.write_text('late save from A'); sys.exit(23)")
        late = subprocess.Popen([binary,"run","--game","world","--",sys.executable,"-c",script,
                                 str(a/"save"),str(ready),str(gate)],env=args("A"))
        try:
            deadline=time.monotonic()+10
            while not ready.exists() and time.monotonic()<deadline: time.sleep(0.03)
            assert ready.exists()
            busy=rpc(url,token,"acquire",game="world",session="late",takeover=None)["Busy"]
            newer=rpc(url,token,"acquire",game="world",session="late",takeover=busy["generation"])["Granted"]
            gate.touch()
            window("Buddy needs attention",["Return"])
            assert late.wait(timeout=10)==23
            assert "1 queued" in call("A","status","world")
            assert "late save from A" == (a/"save").read_text()
            assert call("A","backups","world").count("conflict") >= 4
            assert rpc(url,token,"renew",lease=newer["lease"]) is None
        finally:
            gate.touch()
            if late.poll() is None:
                late.kill();late.wait()
        print("PASS displaced live game keeps its exit code, queued changes and pinned backup",flush=True)
        first_proton_play()
    finally:
        server.terminate();server.wait(timeout=8)


run()
