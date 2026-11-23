mod observe;
mod steam;

use observe::Identity;
use std::{
    env,
    ffi::{OsStr, OsString},
    fs,
    io::{self, Write},
    os::unix::{
        fs::{DirBuilderExt, OpenOptionsExt},
        process::ExitStatusExt,
    },
    path::{Path, PathBuf},
    process::{self, Command, ExitStatus},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const HELP: &str = "buddy-probe libraries [APPID]  Show every registered Steam library
buddy-probe run -- COMMAND...  Wrap Steam's original command
buddy-probe demo               Run a synthetic save and exit
buddy-probe demo-detached      Test a launcher that exits before its child
buddy-probe demo-window        Open a test game window for Steam Game Mode
buddy-probe report [SESSION]   Inspect the latest session, or one by ID
buddy-probe sessions           List all recorded session IDs";

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}
fn home() -> io::Result<PathBuf> {
    let path = env::var_os("BUDDY_PROBE_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|p| Path::new(&p).join(".local/state/buddy-probe")))
        .ok_or_else(|| io::Error::other("HOME is unset"))?;
    if !path.is_absolute() {
        return Err(io::Error::other("probe data directory must be absolute"));
    }
    Ok(path)
}
fn read_optional(path: &Path) -> io::Result<Option<String>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(String::from_utf8_lossy(&bytes).into_owned())),
        Err(e) if e.kind() == io::ErrorKind::NotFound || e.raw_os_error() == Some(libc::ESRCH) => {
            Ok(None)
        }
        Err(e) => Err(e),
    }
}
fn atomic_write(dir: &Path, name: &str, value: &str) -> io::Result<()> {
    let tmp = dir.join(format!(".{name}.{}.tmp", process::id()));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)?;
    file.write_all(value.as_bytes())?;
    file.sync_all()?;
    fs::rename(tmp, dir.join(name))?;
    fs::File::open(dir)?.sync_all()
}
fn event(dir: &Path, name: &str, detail: impl AsRef<str>) -> io::Result<()> {
    let line = format!(
        "{}  {:<18} {}\n",
        now_ms(),
        name,
        detail.as_ref().replace(['\n', '\r'], " ")
    );
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(dir.join("trace"))?;
    file.write_all(line.as_bytes())?;
    file.sync_data()
}
fn session_ids() -> io::Result<Vec<String>> {
    let root = home()?;
    let mut ids = Vec::new();
    if root.is_dir() {
        for entry in fs::read_dir(root)? {
            let entry = entry?;
            let id = entry.file_name().to_string_lossy().into_owned();
            if entry.file_type()?.is_dir() && valid_id(&id) {
                ids.push(id);
            }
        }
    }
    ids.sort();
    Ok(ids)
}
fn valid_id(id: &str) -> bool {
    id.split_once('-').is_some_and(|(t, p)| {
        !t.is_empty() && !p.is_empty() && t.bytes().chain(p.bytes()).all(|b| b.is_ascii_digit())
    })
}
fn report(id: Option<&OsString>) -> io::Result<()> {
    let id = match id {
        Some(id) => id
            .to_str()
            .filter(|id| valid_id(id))
            .map(String::from)
            .ok_or_else(|| io::Error::other("invalid session ID"))?,
        None => session_ids()?
            .pop()
            .ok_or_else(|| io::Error::other("no recorded sessions yet"))?,
    };
    let dir = home()?.join(id);
    println!("Session {}", dir.display());
    print!("{}", fs::read_to_string(dir.join("trace"))?);
    if let Some(info) = read_optional(&dir.join("steam"))? {
        print!("{info}");
    }
    if let Some(receipt) = read_optional(&dir.join("receipt"))? {
        print!("{receipt}");
    } else if let Some(e) = read_optional(&dir.join("error"))? {
        println!("INCONCLUSIVE: {e}");
    } else {
        println!(
            "No receipt yet. Last worker heartbeat (Unix ms): {}",
            read_optional(&dir.join("heartbeat"))?.unwrap_or_else(|| "none".into())
        );
    }
    Ok(())
}
fn exit_code(status: ExitStatus) -> i32 {
    status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(0))
}

fn run(argv: &[OsString]) -> io::Result<i32> {
    let (exe, args) = argv
        .split_first()
        .ok_or_else(|| io::Error::other("missing game command after --"))?;
    let id = format!("{}-{}", now_ms(), process::id());
    let root = home()?;
    fs::DirBuilder::new()
        .mode(0o700)
        .recursive(true)
        .create(&root)?;
    let dir = root.join(&id);
    fs::DirBuilder::new().mode(0o700).create(&dir)?;
    let result = launch(&id, &dir, exe, args);
    if let Err(e) = &result {
        let _ = event(&dir, "wrapper_error", e.to_string());
        let _ = atomic_write(&dir, "error", &e.to_string());
    }
    result
}

fn launch(id: &str, dir: &Path, exe: &OsStr, args: &[OsString]) -> io::Result<i32> {
    let wrapper =
        Identity::of(process::id())?.ok_or_else(|| io::Error::other("cannot identify wrapper"))?;
    atomic_write(dir, "wrapper", &wrapper.encode())?;
    atomic_write(
        dir,
        "pid-namespace",
        &fs::read_link("/proc/self/ns/pid")?.to_string_lossy(),
    )?;
    let app = env::var("SteamAppId")
        .or_else(|_| env::var("SteamGameId"))
        .unwrap_or_default();
    let scope = fs::read_to_string("/proc/self/cgroup")?;
    event(
        dir,
        "wrapper_started",
        format!("pid={} app={} cgroup={}", wrapper.pid, app, scope.trim()),
    )?;
    atomic_write(dir, "steam", &steam::inventory(app.parse().ok()))?;

    let witness = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(dir.join("lifetime.lock"))?;
    witness.lock()?;
    let unit = format!("buddy-probe-{id}");
    let bin = env::current_exe()?;
    let status = Command::new("systemd-run")
        .env_remove("LD_PRELOAD")
        .env_remove("LD_LIBRARY_PATH")
        .args([
            "--user",
            "--quiet",
            "--collect",
            "--service-type=exec",
            "--unit",
        ])
        .arg(&unit)
        .arg("--")
        .arg(&bin)
        .arg("_observe")
        .arg(dir)
        .output()?;
    if !status.status.success() {
        return Err(io::Error::other(format!(
            "could not start {unit}: {}",
            String::from_utf8_lossy(&status.stderr).trim()
        )));
    }
    let start = Instant::now();
    while !dir.join("ready").is_file() {
        if let Some(error) = read_optional(&dir.join("error"))? {
            return Err(io::Error::other(error));
        }
        if start.elapsed() > Duration::from_secs(8) {
            return Err(io::Error::other(format!(
                "observer did not arm; inspect journalctl --user -u {unit}"
            )));
        }
        thread::sleep(Duration::from_millis(25));
    }
    let mut cmd = Command::new(exe);
    cmd.args(args);
    // Only the bundled fixture consumes this path. The game receives its original argv and cwd.
    cmd.env("BUDDY_PROBE_SESSION", dir);
    let mut child = buddy_process::linux::spawn_with_witness(cmd, &witness)?;
    let pid = child.id();
    let register = Identity::of(pid).and_then(|identity| {
        let token = identity
            .map(Identity::encode)
            .unwrap_or_else(|| "already exited".into());
        atomic_write(dir, "launched", &token)?;
        event(dir, "game_spawned", format!("pid={pid}"))
    });
    drop(witness);
    let status = child.wait()?;
    register?;
    let code = exit_code(status);
    atomic_write(dir, "wrapper-result", &format!("game {status}"))?;
    event(dir, "wrapper_return", format!("code={code}"))?;
    Ok(code)
}

fn message_box(title: &str, text: &str) -> io::Result<ExitStatus> {
    match Command::new("zenity")
        .args(["--info", "--no-markup", "--title", title, "--text", text])
        .status()
    {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Command::new("kdialog")
            .args(["--title", title, "--msgbox", text])
            .status(),
        result => result,
    }
}
fn fixture(mode: &OsStr) -> io::Result<()> {
    let dir = PathBuf::from(
        env::var_os("BUDDY_PROBE_SESSION")
            .ok_or_else(|| io::Error::other("run a demo through the wrapper"))?,
    );
    if mode == "demo-detached" {
        let _child = Command::new(env::current_exe()?)
            .args(["_fixture", "demo"])
            .spawn()?;
        return Ok(());
    }
    atomic_write(&dir, "fixture-save", "opened")?;
    event(&dir, "fixture_opened", "synthetic save exists")?;
    if mode == "demo-window" {
        let status = message_box(
            "Buddy test game",
            "The test game is open. Press OK for a normal quit, or use Steam’s Stop Game to test an interrupted exit.",
        )?;
        if !status.success() {
            return Err(io::Error::other(format!("fixture window {status}")));
        }
    } else {
        thread::sleep(Duration::from_secs(3));
    }
    atomic_write(&dir, "fixture-save", "saved-on-quit")?;
    event(&dir, "fixture_saved", "saved-on-quit")
}

fn dispatch() -> io::Result<i32> {
    let args: Vec<_> = env::args_os().skip(1).collect();
    let command = args.first().and_then(|s| s.to_str()).unwrap_or("help");
    match (command, args.len()) {
        ("help" | "--help" | "-h", _) => println!("{HELP}"),
        ("libraries", 1 | 2) => {
            let app = args
                .get(1)
                .map(|s| s.to_string_lossy().parse::<u32>())
                .transpose()
                .map_err(|_| io::Error::other("expected a numeric Steam app ID"))?;
            print!("{}", steam::inventory(app));
        }
        ("report", 1 | 2) => report(args.get(1))?,
        ("sessions", 1) => {
            for id in session_ids()? {
                println!("{id}");
            }
        }
        ("run", _) if args.get(1).is_some_and(|s| s == "--") => return run(&args[2..]),
        ("demo" | "demo-detached" | "demo-window", 1) => {
            return run(&[
                env::current_exe()?.into_os_string(),
                "_fixture".into(),
                args[0].clone(),
            ]);
        }
        ("_observe", 2) => {
            let dir = Path::new(&args[1]);
            if let Err(e) = observe::run(dir) {
                let _ = event(dir, "observer_error", e.to_string());
                let _ = atomic_write(dir, "error", &e.to_string());
                return Err(e);
            }
        }
        ("_fixture", 2) => fixture(&args[1])?,
        _ => return Err(io::Error::other(HELP)),
    }
    Ok(0)
}
fn main() {
    match dispatch() {
        Ok(code) => process::exit(code),
        Err(e) => {
            eprintln!("buddy-probe: {e}");
            if (env::var_os("SteamAppId").is_some() || env::var_os("SteamGameId").is_some())
                && env::args_os()
                    .nth(1)
                    .is_some_and(|s| s == "run" || s == "demo-window")
            {
                let _ = message_box("Buddy launch probe", &e.to_string());
            }
            process::exit(1)
        }
    }
}
