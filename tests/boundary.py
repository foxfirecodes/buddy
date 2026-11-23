"""Run with a real user service manager: python3 tests/boundary.py /path/to/buddy-probe"""
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time


def wait_for(description, read):
    deadline = time.monotonic() + 18
    while time.monotonic() < deadline:
        value = read()
        if value:
            return value
        time.sleep(0.05)
    raise AssertionError(f"timed out waiting for {description}")


def check(binary, root, label, argv, *, kill_wrapper=False, exit_code=0, marker=None):
    home = root / label
    env = dict(os.environ, BUDDY_PROBE_HOME=str(home))
    with subprocess.Popen([binary, *argv], env=env) as process:
        session = wait_for("session", lambda: next(home.glob("*-*"), None))
        if kill_wrapper:
            wait_for("synthetic save", lambda: (session / "fixture-save").exists())
            wait_for("registered launcher", lambda: (session / "launched").exists())
            process.kill()
        assert process.wait(timeout=15) == exit_code, label
    wait_for("completed post-game work", lambda: (session / "receipt").is_file())
    trace = (session / "trace").read_text()
    receipt = (session / "receipt").read_text()
    assert "observer_ready" in trace and "boundary_observed" in trace, trace
    if marker:
        assert f"fixture={marker}\n" in receipt, receipt
        assert trace.index("fixture_saved") < trace.index("boundary_observed"), trace
    if kill_wrapper:
        assert "exit hook missing" in trace and "wrapper_return" not in trace, trace
    if label == "detached":
        assert trace.index("wrapper_return") < trace.index("fixture_saved"), trace
    print(f"PASS {label}", flush=True)


def check_libraries(binary, root):
    steam = root / "Steam"
    external = root / "second library"
    unplugged = root / "missing drive"
    prefix = external / "steamapps/compatdata/42/pfx"
    (steam / "steamapps").mkdir(parents=True)
    prefix.mkdir(parents=True)
    index = "\n".join(f'"{i}" {{ "path" "{p}" }}'
                      for i, p in enumerate([steam, unplugged, external]))
    (steam / "steamapps/libraryfolders.vdf").write_text(f'"libraryfolders" {{ {index} }}')
    (external / "steamapps/appmanifest_42.acf").write_text(
        '"AppState" { "appid" "42" "installdir" "Test Game" }')
    output = subprocess.check_output([binary, "libraries", "42"], text=True,
                                     env=dict(os.environ, BUDDY_STEAM_ROOT=str(steam)))
    assert f"{unplugged} (unavailable" in output, output
    assert str(external / "steamapps/common/Test Game") in output, output
    assert str(prefix) in output, output
    print("PASS second library and offline drive", flush=True)


if __name__ == "__main__":
    binary = Path(sys.argv[1]).resolve(strict=True)
    # Keep the runner alive so the calling terminal cannot tear down orphan fixtures.
    # The user manager needs a host-visible path even if PrivateTmp is enabled.
    with tempfile.TemporaryDirectory(dir=Path("target").resolve()) as directory:
        root = Path(directory)
        check_libraries(binary, root)
        check(binary, root, "normal", ["demo"], marker="saved-on-quit")
        check(binary, root, "detached", ["demo-detached"], marker="saved-on-quit")
        check(binary, root, "killed", ["demo"], kill_wrapper=True, exit_code=-9, marker="saved-on-quit")
        check(binary, root, "arguments", ["run", "--", "/bin/sh", "-c",
              'test "$1" = "two words" && exit 23', "fixture", "two words"], exit_code=23)
