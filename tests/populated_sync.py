"""Benchmark real Buddy clients against a rate-limited server using private disposable data."""
import argparse
from contextlib import contextmanager
import hashlib
import http.client
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import random
import shutil
import sqlite3
import struct
import sys
import subprocess
import tempfile
import threading
import time

from sync import binary, cli
from transport import chunk_keys

MIB = 1024**2
GIB = 1024**3


def inventory(folder):
    result = {}
    for file in sorted(folder.rglob('*')):
        if file.is_symlink() or not (file.is_dir() or file.is_file()):
            raise ValueError('trial worlds must contain only directories and regular files')
        if file.is_file():
            with file.open('rb') as source:
                result[file.relative_to(folder).as_posix()] = (
                    file.stat().st_size, hashlib.file_digest(source, 'sha256').hexdigest())
    return result


def write_region(path, rx, rng):
    # Four-sector chunks in an Anvil container. The root NBT compound carries
    # the chunk coordinates and a unique opaque byte array, using compression ID 3.
    locations = bytearray(4096)
    timestamps = bytearray(4096)
    sectors = 4
    with path.open('wb') as out:
        out.seek(8192)
        for i in range(1024):
            locations[i*4:i*4+4] = struct.pack('>I', ((2+i*sectors) << 8) | sectors)
            timestamps[i*4:i*4+4] = struct.pack('>I', 1_700_000_000)
            prefix = (b'\x0a\0\0\x03\0\x04xPos' + struct.pack('>i', rx*32+i%32)
                      + b'\x03\0\x04zPos' + struct.pack('>i', i//32) + b'\x07\0\x07payload')
            size = sectors*4096 - 5 - len(prefix) - 5
            nbt = prefix + struct.pack('>I', size) + rng.randbytes(size) + b'\0'
            out.write(struct.pack('>IB', 1+len(nbt), 3) + nbt)
        out.seek(0)
        out.write(locations + timestamps)


class Link:
    def __init__(self, address, rate):
        self.address = address
        self.rate = rate * MIB
        self.cut_at = None
        self.clear()

    def clear(self):
        self.upload = self.download = 0
        self.accepted = set()


def handler(link):
    class Proxy(BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def do_POST(self):
            size = int(self.headers['Content-Length'])
            body = self.rfile.read(size)
            writing = self.path == '/v1/packs'
            if writing and link.cut_at is not None and link.upload >= link.cut_at:
                self.send_error(503, 'simulated link interruption')
                return
            if writing:
                time.sleep(size / link.rate)
            headers = {k: v for k, v in self.headers.items() if k.lower() not in ('host', 'connection')}
            conn = http.client.HTTPConnection(link.address, timeout=90)
            try:
                conn.request('POST', self.path, body, headers)
                resp = conn.getresponse()
                data = resp.read()
            finally:
                conn.close()
            if writing and 200 <= resp.status < 300:
                link.upload += size
                link.accepted.update(chunk_keys(body))
            if self.path == '/v1/packs/read' and resp.status == 200:
                time.sleep(len(data)/link.rate)
                link.download += len(data)
            self.send_response(resp.status)
            self.send_header('Content-Length', str(len(data)))
            self.end_headers()
            self.wfile.write(data)
    return Proxy


@contextmanager
def service(root, rate):
    cfg = root/'server.toml'
    cfg.write_text('listen="127.0.0.1:0"\ndatabase="store.sqlite"\n')
    srv = subprocess.Popen([binary, 'serve', '--config', cfg], stderr=subprocess.PIPE, text=True)
    try:
        line = srv.stderr.readline()
        assert line.startswith('Buddy listening on '), line
        link = Link(line.split()[-1], rate)
        with ThreadingHTTPServer(('127.0.0.1', 0), handler(link)) as proxy:
            thread = threading.Thread(target=proxy.serve_forever, daemon=True)
            thread.start()
            try:
                yield cfg, f'http://127.0.0.1:{proxy.server_port}', link
            finally:
                proxy.shutdown()
                thread.join()
    finally:
        srv.terminate()
        srv.wait(timeout=10)


def setup(profile, world, cfg, endpoint, new=False):
    flags = ['--invite', cli(profile, 'invite', '--config', cfg).strip()] if new else []
    cli(profile, 'login', endpoint, 'tester', '--device', profile.name, '--password-stdin',
        '--private-file-credential', *flags, input='private-benchmark\n')
    cli(profile, 'setup', 'world', world)


def measure_client(command, env):
    """Count bytes read by the Linux client, including reads served from page cache."""
    counts = {}
    with tempfile.TemporaryFile(mode='w+t') as log:
        started = time.monotonic()
        with subprocess.Popen(command, env=env, stdout=log, stderr=log) as proc:
            if sys.platform == 'linux':
                # The kernel retains final I/O counters until wait() reaps the child.
                os.waitid(os.P_PID, proc.pid, os.WEXITED | os.WNOWAIT)
                try:
                    lines = Path(f'/proc/{proc.pid}/io').read_text().splitlines()
                except OSError:
                    pass  # procfs counters can be restricted by the host.
                else:
                    io = dict(line.split(': ') for line in lines)
                    counts = {f'client_{key}': int(io[key]) for key in ('rchar', 'read_bytes') if key in io}
            code = proc.wait()
            seconds = round(time.monotonic() - started, 3)
        log.seek(0)
        return code, log.read(), seconds, counts


def trial(root, args, report):
    a, b = root/'a', root/'b'
    src, dst = root/'source', root/'destination'
    src.mkdir()
    dst.mkdir()
    (src/'baseline').write_bytes(b'complete old revision')
    with service(root, args.mib_per_sec) as (cfg, endpoint, link):
        setup(a, src, cfg, endpoint, True)
        setup(b, dst, cfg, endpoint)
        cli(a, 'sync', 'world')
        cli(b, 'sync', 'world')
        baseline = inventory(dst)
        if args.world:
            original = inventory(args.world)
            shutil.copytree(args.world, src/'Minecraft', symlinks=True)
            assert inventory(src/'Minecraft') == original
            report['minecraft_bytes'] = sum(n for n, _ in original.values())
            report['minecraft_files'] = len(original)
        print('Preparing supplemental data', flush=True)
        additional = src/'Supplemental regions'
        additional.mkdir()
        rng = random.Random(421)
        total = sum(n for n, _ in inventory(src).values())
        required = args.gib * GIB
        region = 0
        # Even a mostly or entirely duplicate input must produce several unique
        # packs on each side of the simulated 32 MiB interruption.
        while total < required or region < 3:
            path = additional/f'r.{region}.0.mca'
            write_region(path, region, rng)
            total += path.stat().st_size
            region += 1
        expected = inventory(src)
        report.update(bytes=sum(n for n, _ in expected.values()), files=len(expected),
                      supplemental_regions=region, rate_mib_per_sec=args.mib_per_sec)
        print('Prepared', report['bytes'], 'bytes in', report['files'], 'files', flush=True)

        def sync(profile, label, succeeds=True, launch=False):
            link.clear()
            stamp = root/'game-entry'
            if launch:
                stamp.unlink(missing_ok=True)
                clock = "import sys,time; open(sys.argv[1],'w').write(str(time.monotonic()))"
                args = ['run', '--game', 'world', '--', sys.executable, '-c', clock, stamp]
            else:
                args = ['sync', 'world']
            started = time.monotonic()
            environment = os.environ.copy()
            if environment.get('BUDDY_TEST_X11') != 'isolated':
                # This benchmark may start a progress helper. Leave the primary desktop alone.
                for key in ('DISPLAY', 'WAYLAND_DISPLAY', 'WAYLAND_SOCKET'):
                    environment.pop(key, None)
            code, output, seconds, counts = measure_client([binary, '--profile-dir', profile, *args], environment)
            metrics = dict(seconds=seconds, uploaded=link.upload, downloaded=link.download, **counts)
            assert (code == 0) == succeeds, output[-1000:]
            if not succeeds:
                assert '503' in output, output
            if launch:
                metrics['until_game_entry_seconds'] = round(float(stamp.read_text()) - started, 3)
            print(label, metrics, flush=True)
            report[label] = metrics

        with sqlite3.connect(root/'store.sqlite') as db:
            def head():
                return db.execute('SELECT head FROM games').fetchone()[0]
            before = head()
            link.cut_at = 32 * MIB
            sync(a, 'interrupted', False)
            acknowledged = link.accepted.copy()
            assert acknowledged and head() == before
            cli(b, 'sync', 'world')
            assert inventory(dst) == baseline
            link.cut_at = None
            sync(a, 'resume_upload')
            assert not (acknowledged & link.accepted) and head() != before
            sync(b, 'fresh_restore')
            assert inventory(dst) == expected
            for sample in range(1, 6):
                sync(a, f'warm_launch_{sample}', launch=True)
                assert link.upload == link.download == 0

            # Modify an existing chunk timestamp in the copied region header, leaving chunk content intact.
            real_regions = sorted((src/'Minecraft').rglob('r.*.mca'))
            candidates = real_regions or sorted(additional.glob('*.mca'))
            path = max(candidates, key=lambda p: p.stat().st_size)
            with path.open('r+b') as file:
                locations = file.read(4096)
                i = next(i for i in range(1024) if any(locations[i*4:i*4+3]))
                file.seek(4096+4*i)
                timestamp = struct.unpack('>I', file.read(4))[0] ^ 1
                file.seek(4096+4*i)
                file.write(struct.pack('>I', timestamp))
            sync(a, 'region_edit')
            assert 0 < link.upload < max(path.stat().st_size+68, 8*MIB)
            sync(b, 'region_update')
            expected = inventory(src)
            assert inventory(dst) == expected
            report['tree_sha256'] = hashlib.sha256(json.dumps(expected, sort_keys=True).encode()).hexdigest()
            if args.world:
                assert inventory(args.world) == original
            report['passed'] = True


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('binary')
    parser.add_argument('--world', type=Path, help='Optional closed Minecraft world to copy read-only')
    parser.add_argument('--gib', type=float, default=2.0, help='minimum total fixture size in GiB')
    parser.add_argument('--mib-per-sec', type=int, default=48)
    parser.add_argument('--report', type=Path)
    args = parser.parse_args()
    if args.gib <= 0 or args.mib_per_sec <= 0:
        parser.error('size and bandwidth must be positive')
    scratch = Path('target').resolve()
    estimated = max(args.gib*GIB, sum(f.stat().st_size for f in args.world.rglob('*') if f.is_file()) if args.world else 0)
    if estimated < 64*MIB:
        parser.error('select a workload of at least 64 MiB to exercise a multi-pack interruption')
    if shutil.disk_usage(scratch).free < 6*estimated + 2*GIB:
        parser.error('insufficient free space in target for both worlds, both caches and the server')
    metrics = {}
    with tempfile.TemporaryDirectory(dir=scratch, prefix='populated-trial-') as tmp:
        trial(Path(tmp), args, metrics)
    if args.report:
        args.report.write_text(json.dumps(metrics, indent=2)+'\n')
