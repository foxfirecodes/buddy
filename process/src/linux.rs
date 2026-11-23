use std::{
    collections::BTreeSet,
    fs, io,
    os::unix::{io::AsRawFd, process::CommandExt},
    path::Path,
    process::{Command, ExitStatus},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};
fn read_optional(path: &Path) -> io::Result<Option<String>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(String::from_utf8_lossy(&bytes).into_owned())),
        Err(e) if e.kind() == io::ErrorKind::NotFound || e.raw_os_error() == Some(libc::ESRCH) => {
            Ok(None)
        }
        Err(e) => Err(e),
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Identity {
    pub pid: u32,
    start: u64,
}

impl Identity {
    pub fn of(pid: u32) -> io::Result<Option<Self>> {
        let Some(stat) = read_optional(Path::new(&format!("/proc/{pid}/stat")))? else {
            return Ok(None);
        };
        // comm can contain spaces and ')'. Field 22 is the start tick since boot.
        let tail = stat
            .rsplit_once(')')
            .ok_or_else(|| io::Error::other("invalid proc stat"))?
            .1;
        let mut fields = tail.split_whitespace();
        if matches!(fields.next(), Some("Z" | "X")) {
            return Ok(None);
        }
        let start = fields
            .nth(18)
            .and_then(|v| v.parse().ok())
            .ok_or_else(|| io::Error::other("missing process start tick"))?;
        Ok(Some(Self { pid, start }))
    }
    pub fn encode(self) -> String {
        format!("{} {}", self.pid, self.start)
    }
    pub fn decode(s: &str) -> io::Result<Self> {
        let invalid = || io::Error::other("invalid recorded process identity");
        let (pid, start) = s.trim().split_once(' ').ok_or_else(invalid)?;
        Ok(Self {
            pid: pid.parse().map_err(|_| invalid())?,
            start: start.parse().map_err(|_| invalid())?,
        })
    }
    pub fn alive(self) -> io::Result<bool> {
        Ok(Self::of(self.pid)? == Some(self))
    }
}

// Remember children seen at least once, even if they later get reparented.
// A child which both detaches and closes inherited fds between samples can escape observation.
pub fn sample(known: &mut BTreeSet<Identity>) -> io::Result<()> {
    let mut todo: Vec<_> = known.iter().copied().collect();
    let mut live = BTreeSet::new();
    while let Some(parent) = todo.pop() {
        if live.contains(&parent) || !parent.alive()? {
            continue;
        }
        live.insert(parent);
        let threads = match fs::read_dir(format!("/proc/{}/task", parent.pid)) {
            Ok(threads) => threads,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        for entry in threads {
            let children = entry?.path().join("children");
            if let Some(pids) = read_optional(&children)? {
                for pid in pids.split_whitespace().filter_map(|s| s.parse().ok()) {
                    if let Some(child) = Identity::of(pid)? {
                        todo.push(child);
                    }
                }
            }
        }
    }
    *known = live;
    Ok(())
}

// Inherit a shared open file description across exec, including a quick launcher handoff.
pub fn spawn_with_witness(
    mut command: Command,
    witness: &fs::File,
) -> io::Result<std::process::Child> {
    let fd = witness.as_raw_fd();
    // SAFETY: the caller keeps the file open until spawn and fcntl is async-signal-safe.
    unsafe {
        command.pre_exec(move || {
            if libc::fcntl(fd, libc::F_SETFD, 0) == -1 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    command.spawn()
}
pub fn wait_for_game(command: Command, started: impl FnOnce()) -> io::Result<ExitStatus> {
    let file = tempfile::NamedTempFile::new()?;
    let observer = file.reopen()?;
    file.as_file().lock()?;
    let term = Arc::new(AtomicBool::new(false));
    let interrupt = Arc::new(AtomicBool::new(false));
    struct Signals(Vec<signal_hook::SigId>);
    impl Drop for Signals {
        fn drop(&mut self) {
            for id in self.0.drain(..) {
                signal_hook::low_level::unregister(id);
            }
        }
    }
    let mut signals = Signals(vec![]);
    for (sig, flag) in [(libc::SIGTERM, &term), (libc::SIGINT, &interrupt)] {
        signals
            .0
            .push(signal_hook::flag::register(sig, flag.clone())?);
    }
    let mut child = spawn_with_witness(command, file.as_file())?;
    started();
    drop(file);
    let mut known = BTreeSet::new();
    if let Some(identity) = Identity::of(child.id())? {
        known.insert(identity);
    }
    let mut returned = None;
    loop {
        sample(&mut known)?;
        if returned.is_none() {
            for (sig, flag) in [(libc::SIGTERM, &term), (libc::SIGINT, &interrupt)] {
                if flag.swap(false, Ordering::Relaxed) {
                    // SAFETY: this child has not yet been reaped, so its PID cannot be recycled.
                    unsafe {
                        libc::kill(child.id() as i32, sig);
                    }
                }
            }
            returned = child.try_wait()?;
        }
        if let Some(status) = returned
            && known.is_empty()
        {
            match observer.try_lock() {
                Ok(()) => return Ok(status),
                Err(fs::TryLockError::WouldBlock) => (),
                Err(fs::TryLockError::Error(e)) => return Err(e),
            }
        }
        thread::sleep(Duration::from_millis(180));
    }
}
