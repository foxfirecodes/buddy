"""Optional Linux benchmark. Run against a release build on a volume with 12 GiB free."""
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import http.client
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import threading
import time

binary = str(Path(sys.argv[1]).resolve(strict=True))
with tempfile.TemporaryDirectory(dir=Path("target").resolve()) as name:
    root = Path(name)
    assert shutil.disk_usage(root).free > 12 * 1024**3
    cfg = root / "server.toml"
    cfg.write_text('listen="127.0.0.1:0"\ndatabase="bench.sqlite"\n')
    server = subprocess.Popen([binary,"server","serve","--config",str(cfg)],stderr=subprocess.PIPE,text=True)
    addr=server.stderr.readline().strip().split()[-1]
    sent=received=0

    class CountChunks(BaseHTTPRequestHandler):
        def do_POST(self):
            global sent,received
            data=self.rfile.read(int(self.headers["Content-Length"]))
            conn=http.client.HTTPConnection(addr, timeout=90)
            conn.request("POST",self.path,data,{k:v for k,v in self.headers.items() if k.lower()!="host"})
            r=conn.getresponse()
            answer=r.read()
            if self.path=="/v1/packs":
                sent+=len(data)
            if self.path=="/v1/packs/read":
                received+=len(answer)
            self.send_response(r.status)
            self.send_header("Content-Length",str(len(answer)))
            self.end_headers()
            self.wfile.write(answer)
            conn.close()
        def log_message(self,*args):
            pass

    proxy=ThreadingHTTPServer(("127.0.0.1",0),CountChunks)
    threading.Thread(target=proxy.serve_forever,daemon=True).start()
    url=f"http://127.0.0.1:{proxy.server_port}"
    def cmd(device,*args,data=None):
        return subprocess.check_output([binary,*map(str,args)],input=data,text=True,env=dict(os.environ,BUDDY_HOME=str(root/device),BUDDY_ALLOW_FILE_CREDENTIAL="true"))
    def bench(label,device):
        global received,sent
        received=sent=0
        tic=time.monotonic()
        proc=subprocess.Popen([binary,"sync","big"],env=dict(os.environ,BUDDY_HOME=str(root/device)))
        _,status,usage=os.wait4(proc.pid,0)
        proc.returncode=os.waitstatus_to_exitcode(status)
        assert proc.returncode==0
        print(f"{label}: {time.monotonic()-tic:.2f}s, {sent} bytes uploaded, {received} bytes downloaded, {usage.ru_maxrss/1024:.1f} MiB peak RSS",flush=True)
        return sent,received
    try:
        a,b=root/"world-a",root/"world-b"
        a.mkdir();b.mkdir()
        for device,folder in [("a",a),("b",b)]:
            invite=["--invite",cmd(device,"server","invite","--config",cfg).strip()] if device=="a" else []
            cmd(device,"login",url,"bench","--password-stdin",*invite,data="test-passphrase\n")
            cmd(device,"setup","big",folder)
        with (a/"regions").open("wb") as f:
            f.truncate(10*1024**3)
        for i in range(2000):
            (a/f"player-{i}").write_text(str(i))
        # Model a world already present at launch. Recently written files must miss the scan cache.
        time.sleep(2.2)
        bench("Initial sparse 10 GiB world plus 2,000 files","a")
        assert bench("Unchanged sync","a")==(0,0)
        with (a/"regions").open("r+b") as f:
            f.seek(5*1024**3+123);f.write(b"changed")
        uploaded,_=bench("Seven-byte edit at 5 GiB","a")
        assert 0<uploaded<10*1024**2
        bench("Fresh device restore","b")
        with (b/"regions").open("rb") as f:
            f.seek(5*1024**3+123);assert f.read(7)==b"changed"
        assert (b/"regions").stat().st_size==10*1024**3 and (b/"player-1999").read_text()=="1999"
        print("PASS restore content, length and tiny-file inventory",flush=True)
    finally:
        proxy.shutdown();proxy.server_close()
        server.terminate();server.wait(timeout=15)
