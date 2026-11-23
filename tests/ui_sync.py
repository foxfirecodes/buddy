"""Run under xvfb-run on an isolated display. Exercise decisions with the shipping Linux GUI."""
import json
import os
import re
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


def run():
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
        resolving=play("A",a,"after choosing server")
        window("Which save should Buddy continue with?",["Left","Return"])
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
    finally:
        server.terminate();server.wait(timeout=8)


run()
