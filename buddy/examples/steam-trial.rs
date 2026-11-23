//! Disposable launch-boundary acceptance test. Run beside buddy[.exe], or pass --buddy.
use anyhow::{Context, Result, bail, ensure};
use clap::{Parser, Subcommand};
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};
use uuid::Uuid;

#[derive(Parser)]
struct Args {
    #[arg(long)]
    buddy: Option<PathBuf>,
    /// Write the outcome to a new JSON file.
    #[arg(long, global = true)]
    report: Option<PathBuf>,
    #[command(subcommand)]
    mode: Mode,
}
#[derive(Subcommand)]
enum Mode {
    /// Exercise the wrapper directly, including its exit code.
    Direct {
        /// Exercise the graphical conflict chooser as part of the direct test.
        #[arg(long)]
        conflict: bool,
    },
    /// Wait for Play on a real Steam non-Steam shortcut.
    Shortcut {
        /// Require the real graphical conflict chooser before the game starts.
        #[arg(long)]
        conflict: bool,
    },
    /// Wait for Play on a real installed Steam store title.
    Store { app_id: u32 },
    #[command(hide = true)]
    Launcher { root: PathBuf, argument: String },
    #[command(hide = true)]
    Writer { root: PathBuf },
}

const ARGUMENT: &str = "two words 雪";
const GAME: &str = "buddy-trial";

fn until(
    timeout: Duration,
    message: &str,
    mut condition: impl FnMut() -> Result<bool>,
) -> Result<()> {
    let start = Instant::now();
    while !condition()? {
        ensure!(start.elapsed() < timeout, "Timed out waiting for {message}");
        thread::sleep(Duration::from_millis(80));
    }
    Ok(())
}

// A launcher that exits well before its child writes the save.
fn launcher(root: &Path, argument: &str) -> Result<()> {
    ensure!(
        argument == ARGUMENT,
        "Steam changed the quoted argument: {argument:?}"
    );
    ensure!(
        std::env::current_dir()?.canonicalize()?
            == root.join("Working Directory").canonicalize()?,
        "unexpected game working directory"
    );
    let seed = fs::read(root.join("seed"))?;
    ensure!(
        fs::read(root.join("player-world/level.sav"))? == seed,
        "save was not restored before launch"
    );
    let steam = std::env::var("SteamGameId")
        .ok()
        .or_else(|| std::env::var("SteamAppId").ok());
    fs::write(
        root.join("launch.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "working_directory": std::env::current_dir()?, "argument": argument, "steam_id": steam,
        }))?,
    )?;
    Command::new(std::env::current_exe()?)
        .arg("writer")
        .arg(root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    Ok(())
}
fn writer(root: &Path) -> Result<()> {
    fs::write(root.join("ready"), b"ready")?;
    until(
        Duration::from_secs(45),
        "permission to finish the synthetic game",
        || Ok(root.join("finish").is_file()),
    )?;
    fs::write(
        root.join("player-world/level.sav"),
        fs::read(root.join("next"))?,
    )?;
    Ok(())
}

struct Server(Child);
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
struct Trial {
    root: PathBuf,
    buddy: PathBuf,
    game: String,
}
impl Trial {
    fn client(&self, device: &str) -> Command {
        let mut c = Command::new(&self.buddy);
        c.env("BUDDY_HOME", self.root.join(device))
            .env_remove("BUDDY_INVITE");
        c
    }
    fn success(&self, device: &str, args: &[&str]) -> Result<String> {
        check(self.client(device).args(args).output()?)
    }
    fn syncing(&self) -> Result<Output> {
        Ok(self.client("witness").args(["sync", &self.game]).output()?)
    }
    fn start_server(&self) -> Result<(Server, String)> {
        fs::write(
            self.root.join("server.toml"),
            "listen='127.0.0.1:0'\ndatabase='trial.sqlite'\n",
        )?;
        let mut server = Server(
            Command::new(&self.buddy)
                .arg("serve")
                .arg("--config")
                .arg(self.root.join("server.toml"))
                .stderr(Stdio::piped())
                .spawn()?,
        );
        let mut reader = BufReader::new(server.0.stderr.take().context("server stderr")?);
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let mut first = String::new();
            let _ = reader.read_line(&mut first);
            let _ = tx.send(first);
            for l in reader.lines().map_while(std::result::Result::ok) {
                eprintln!("{l}");
            }
        });
        let first = rx.recv_timeout(Duration::from_secs(30))?;
        let addr = first
            .trim()
            .strip_prefix("Buddy listening on ")
            .with_context(|| format!("starting server: {first}"))?;
        Ok((server, format!("http://{addr}")))
    }
    fn prepare(&self, url: &str) -> Result<()> {
        let invite = check(
            Command::new(&self.buddy)
                .arg("invite")
                .arg("--config")
                .arg(self.root.join("server.toml"))
                .output()?,
        )?;
        let password = Uuid::new_v4().to_string();
        fs::create_dir(self.root.join("Working Directory"))?;
        for device in ["witness", "player"] {
            let world = self.root.join(format!("{device}-world"));
            fs::create_dir(&world)?;
            let mut login = self.client(device);
            login.args([
                "login",
                url,
                "tester",
                "--device",
                device,
                "--password-stdin",
                "--private-file-credential",
            ]);
            if device == "witness" {
                login.env("BUDDY_INVITE", invite.trim());
            }
            let mut proc = login.stdin(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
            writeln!(proc.stdin.take().context("login stdin")?, "{password}")?;
            check(proc.wait_with_output()?)?;
            check(
                self.client(device)
                    .args(["setup", &self.game])
                    .arg(world)
                    .output()?,
            )?;
        }
        let seed = Uuid::new_v4().to_string();
        fs::write(self.root.join("seed"), &seed)?;
        fs::write(self.root.join("next"), Uuid::new_v4().to_string())?;
        fs::write(self.root.join("witness-world/level.sav"), seed)?;
        self.success("witness", &["sync", &self.game])?;
        Ok(())
    }
    fn busy(&self) -> Result<bool> {
        let out = self.syncing()?;
        if out.status.success() {
            return Ok(false);
        }
        let error = String::from_utf8_lossy(&out.stderr);
        ensure!(
            error.contains("Another device has this game open"),
            "witness sync failed: {error}"
        );
        Ok(true)
    }
    fn verify_final(&self) -> Result<()> {
        until(
            Duration::from_secs(30),
            "post-game publication and lease release",
            || Ok(!self.busy()?),
        )?;
        ensure!(
            fs::read(self.root.join("witness-world/level.sav"))?
                == fs::read(self.root.join("next"))?,
            "the second profile did not receive the late save"
        );
        Ok(())
    }
    fn run(&self, mode: &Mode) -> Result<()> {
        let exe = std::env::current_exe()?;
        let (_server, url) = self.start_server()?;
        self.prepare(&url)?;
        let conflict = matches!(
            mode,
            Mode::Shortcut { conflict: true } | Mode::Direct { conflict: true }
        );
        if conflict {
            self.success("player", &["sync", &self.game])?;
            fs::write(
                self.root.join("player-world/level.sav"),
                "independent device edit",
            )?;
            let remote = Uuid::new_v4().to_string();
            fs::write(self.root.join("witness-world/level.sav"), &remote)?;
            self.success("witness", &["sync", &self.game])?;
            fs::write(self.root.join("seed"), remote)?;
            println!(
                "At the Buddy conflict window choose Use server copy. The game checks the exact restored bytes."
            );
        }
        let working = self.root.join("Working Directory");
        let profile = self.root.join("player");
        println!("Trial directory: {}", self.root.display());
        let mut wrapped = None;
        match mode {
            Mode::Direct { .. } => {
                wrapped = Some(
                    self.client("player")
                        .args(["run", "--game", &self.game, "--"])
                        .arg(&exe)
                        .arg("launcher")
                        .arg(&self.root)
                        .arg(ARGUMENT)
                        .current_dir(&working)
                        .env_remove("SteamGameId")
                        .env_remove("SteamAppId")
                        .spawn()?,
                );
            }
            Mode::Shortcut { .. } => {
                println!(
                    "Create a temporary non-Steam shortcut. Paste these fields and press Play.\nTarget: \"{}\"\nStart In: \"{}\"\nLaunch Options: --profile-dir \"{}\" run --game {} -- \"{}\" launcher \"{}\" \"{ARGUMENT}\"",
                    self.buddy.display(),
                    working.display(),
                    profile.display(),
                    self.game,
                    exe.display(),
                    self.root.display()
                );
            }
            Mode::Store { app_id } => {
                println!(
                    "Save the original Launch Options for Steam app {app_id}. For this trial, start with an empty field. Paste the line below and press Play.\n\"{}\" --profile-dir \"{}\" run -- %command%\nWhen the game is visible, return here and press Enter.",
                    self.buddy.display(),
                    profile.display()
                );
                let mut line = String::new();
                ensure!(
                    std::io::stdin().read_line(&mut line)? > 0,
                    "missing confirmation that the game is open"
                );
                ensure!(self.busy()?, "Steam launch did not hold the trial lease");
                ensure!(
                    fs::read(self.root.join("player-world/level.sav"))?
                        == fs::read(self.root.join("seed"))?,
                    "initial pull failed"
                );
                fs::write(
                    self.root.join("player-world/level.sav"),
                    fs::read(self.root.join("next"))?,
                )?;
                println!("Lease and initial pull verified. Quit the game, then press Enter here.");
                ensure!(
                    std::io::stdin().read_line(&mut line)? > 0,
                    "missing quit confirmation"
                );
                self.verify_final()?;
                println!(
                    "PASS Steam store title held the lease and published the disposable save on exit. Restore its original Launch Options."
                );
                return Ok(());
            }
            _ => unreachable!(),
        }
        until(
            Duration::from_secs(if wrapped.is_some() && !conflict {
                30
            } else {
                600
            }),
            "the child game to start",
            || {
                if let Ok(message) = fs::read_to_string(self.root.join("fixture-error")) {
                    bail!("game fixture: {message}");
                }
                if let Some(child) = &mut wrapped
                    && let Some(status) = child.try_wait()?
                {
                    bail!("wrapper exited before the child was released: {status}");
                }
                Ok(self.root.join("ready").is_file())
            },
        )?;
        // Let the short-lived launcher exit while its child stays gated. The final byte
        // comparison also catches an early close racing the busy check.
        thread::sleep(Duration::from_millis(400));
        let held = self.busy();
        fs::write(self.root.join("finish"), b"finish")?;
        ensure!(
            held?,
            "Buddy released the lease while the child game was still open"
        );
        if let Some(mut child) = wrapped {
            until(Duration::from_secs(30), "the wrapper to exit", || {
                Ok(child.try_wait()?.is_some())
            })?;
            ensure!(
                child.wait()?.code() == Some(23),
                "launcher exit code 23 was lost"
            );
        } else {
            let report: serde_json::Value =
                serde_json::from_slice(&fs::read(self.root.join("launch.json"))?)?;
            ensure!(
                report["steam_id"].as_str().is_some_and(|v| !v.is_empty()),
                "no Steam launch context recorded; repeat from Steam"
            );
        }
        self.verify_final()?;
        if conflict {
            let backups = self.success("witness", &["backups", &self.game])?;
            ensure!(
                backups
                    .lines()
                    .filter(|line| line.contains("conflict"))
                    .count()
                    == 2,
                "the two permanent conflict backups are missing"
            );
            println!(
                "PASS graphical choice restored the selected version and preserved both backups."
            );
        }
        println!(
            "PASS restored before launch, preserved cwd and arguments, held the lease for a child and delivered its final save."
        );
        Ok(())
    }
}
fn check(out: Output) -> Result<String> {
    ensure!(
        out.status.success(),
        "Buddy returned {}: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(String::from_utf8(out.stdout)?)
}
fn run() -> Result<i32> {
    let args = Args::parse();
    match args.mode {
        Mode::Launcher { root, argument } => {
            if let Err(error) = launcher(&root, &argument) {
                let _ = fs::write(root.join("fixture-error"), format!("{error:#}"));
                return Err(error);
            }
            Ok(23)
        }
        Mode::Writer { root } => {
            writer(&root)?;
            Ok(0)
        }
        mode => {
            let exe = std::env::current_exe()?;
            let buddy = args.buddy.unwrap_or_else(|| {
                exe.with_file_name(format!("buddy{}", std::env::consts::EXE_SUFFIX))
            });
            ensure!(
                buddy.is_file(),
                "place Buddy next to this executable or pass --buddy PATH"
            );
            let dir = tempfile::Builder::new()
                .prefix("Buddy Steam Trial ")
                .tempdir()?;
            let game = if let Mode::Store { app_id } = mode {
                format!("steam-{app_id}")
            } else {
                GAME.into()
            };
            let trial = Trial {
                root: dir.path().to_owned(),
                buddy: std::path::absolute(buddy)?,
                game,
            };
            let report = args
                .report
                .map(|path| {
                    fs::File::create_new(&path)
                        .with_context(|| format!("create new report {}", path.display()))
                })
                .transpose()?;
            let result = trial.run(&mode);
            if result.is_err() {
                let _ = fs::write(trial.root.join("finish"), b"finish");
            }
            if let Some(file) = report {
                let origin = match &mode {
                    Mode::Direct { conflict: true } => "direct-conflict",
                    Mode::Direct { conflict: false } => "direct",
                    Mode::Shortcut { conflict: true } => "steam-shortcut-conflict",
                    Mode::Shortcut { conflict: false } => "steam-shortcut",
                    Mode::Store { .. } => "steam-store",
                    _ => unreachable!(),
                };
                let observed = fs::read(trial.root.join("launch.json"))
                    .ok()
                    .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok());
                let record = serde_json::json!({
                    "test":origin, "game":trial.game,"target_os":std::env::consts::OS,
                    "result":if result.is_ok() {"passed"} else {"failed"},
                    "observed_steam_id":observed.and_then(|v|v.get("steam_id").cloned()),
                    "error":result.as_ref().err().map(|e|format!("{e:#}")),
                });
                serde_json::to_writer_pretty(file, &record)?;
            }
            result?;
            Ok(0)
        }
    }
}
fn main() {
    match run() {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("FAIL {e:#}");
            std::process::exit(1);
        }
    }
}
