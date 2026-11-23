"""Hit real ENOSPC in a private 16 MiB Linux tmpfs and recover the real client."""
import os
from pathlib import Path
import sqlite3
import subprocess
import sys
import tempfile

from populated_sync import MIB, service, setup
from sync import binary, cli


def contents(world):
    return {p.name: p.read_bytes() for p in world.iterdir() if p.is_file()}


def prove(root):
    disk = root / 'bounded'
    subprocess.run(['mount', '-t', 'tmpfs', '-o', 'size=16m,mode=0700', 'tmpfs', disk], check=True)
    a, b = root/'client-a', root/'client-b'
    src, dst = root/'source', disk/'world'
    src.mkdir()
    dst.mkdir()
    (src/'level.dat').write_bytes(b'old level')
    (src/'old-region').write_bytes(b'previous region')
    with service(root, 128) as (cfg, endpoint, _):
        setup(a, src, cfg, endpoint, True)
        setup(b, dst, cfg, endpoint)
        cli(a, 'sync', 'world')
        cli(b, 'sync', 'world')
        before = contents(dst)
        (src/'level.dat').write_bytes(b'new level')
        (src/'old-region').unlink()
        (src/'new-region').write_bytes(os.urandom(6*MIB))
        cli(a, 'sync', 'world')
        with (disk/'ballast').open('xb') as f:
            os.posix_fallocate(f.fileno(), 0, 13*MIB)

        with sqlite3.connect(root/'store.sqlite') as db:
            def head():
                return db.execute('SELECT head FROM games').fetchone()[0]
            committed = head()
            for rollback in [True, False]:
                msg = cli(b, 'sync', 'world', ok=False)
                assert 'os error 28' in msg, msg
                assert contents(dst) == before
                assert head() == committed and 'restore pending=true' in cli(b, 'status', 'world')
                stages = list(dst.glob('.buddy-stage-*'))
                assert len(stages) == 1 and any(stages[0].iterdir())
                if rollback:
                    cli(b, 'rollback', 'world')
                    assert contents(dst) == before and not list(dst.glob('.buddy-stage-*'))
                    assert 'restore pending=false' in cli(b, 'status', 'world')
            (disk/'ballast').unlink()
            cli(b, 'sync', 'world')
            assert contents(dst) == contents(src) and head() == committed
            assert not list(dst.glob('.buddy-stage-*')) and 'restore pending=false' in cli(b, 'status', 'world')
            print('PASS ENOSPC after staging began, old files preserved, rollback while full, retry after freeing space')


if __name__ == '__main__':
    if len(sys.argv) == 4 and sys.argv[2] == '--private-mount':
        prove(Path(sys.argv[3]))
    else:
        with tempfile.TemporaryDirectory(prefix='buddy-capacity-') as tmp:
            root = Path(tmp)
            (root/'bounded').mkdir()
            subprocess.run(['unshare', '--user', '--map-root-user', '--mount', '--propagation', 'private',
                            sys.executable, __file__, binary, '--private-mount', root], check=True)
