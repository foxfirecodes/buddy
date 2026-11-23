use crate::{atomic_write, event, read_optional};
use std::{
    collections::BTreeSet,
    fs, io,
    path::Path,
    process, thread,
    time::{Duration, Instant},
};

pub use buddy_process::linux::Identity;
use buddy_process::linux::sample;

pub fn run(dir: &Path) -> io::Result<()> {
    let wrapper = Identity::decode(&fs::read_to_string(dir.join("wrapper"))?)?;
    let ns = fs::read_link("/proc/self/ns/pid")?;
    if ns != Path::new(&fs::read_to_string(dir.join("pid-namespace"))?) || !wrapper.alive()? {
        return Err(io::Error::other(
            "worker and wrapper need the same visible PID namespace",
        ));
    }
    let scope = fs::read_to_string("/proc/self/cgroup")?;
    event(
        dir,
        "observer_ready",
        format!("pid={} cgroup={}", process::id(), scope.trim()),
    )?;
    atomic_write(dir, "ready", "ready")?;
    let witness = fs::File::open(dir.join("lifetime.lock"))?;
    let mut known = BTreeSet::new();
    let mut launcher_registered = false;
    let mut wrapper_gone = false;
    let mut quiet_since = None;
    let mut heartbeat = Instant::now();
    let mut last_evidence = None;
    let mut last_poll = crate::now_ms();
    loop {
        let now = crate::now_ms();
        let gap = now.saturating_sub(last_poll);
        if gap > 5_000 {
            event(
                dir,
                "sampling_gap",
                format!("{gap} ms between samples (wall clock)"),
            )?;
            quiet_since = None;
        }
        last_poll = now;
        if !launcher_registered && let Some(text) = read_optional(&dir.join("launched"))? {
            launcher_registered = true;
            if text != "already exited" {
                known.insert(Identity::decode(&text)?);
            }
        }
        sample(&mut known)?;
        let held = match witness.try_lock() {
            Ok(()) => {
                witness.unlock()?;
                false
            }
            Err(fs::TryLockError::WouldBlock) => true,
            Err(fs::TryLockError::Error(e)) => return Err(e),
        };
        let wrapper_live = wrapper.alive()?;
        if !wrapper_live && !wrapper_gone {
            wrapper_gone = true;
            let result = read_optional(&dir.join("wrapper-result"))?
                .unwrap_or_else(|| "exit hook missing".into());
            event(dir, "wrapper_gone", result)?;
        }
        let evidence = (wrapper_live, held, known.len());
        if last_evidence != Some(evidence) {
            event(
                dir,
                "activity",
                format!(
                    "wrapper={wrapper_live} inherited_lock={held} tracked_processes={}",
                    known.len()
                ),
            )?;
            last_evidence = Some(evidence);
        }
        if heartbeat.elapsed() >= Duration::from_secs(5) {
            atomic_write(dir, "heartbeat", &crate::now_ms().to_string())?;
            heartbeat = Instant::now();
        }
        if wrapper_gone && !held && known.is_empty() {
            let start = quiet_since.get_or_insert_with(Instant::now);
            if start.elapsed() >= Duration::from_secs(2) {
                if !launcher_registered {
                    return Err(io::Error::other(
                        "no confirmed launch before wrapper disappeared",
                    ));
                }
                break;
            }
        } else {
            quiet_since = None;
        }
        thread::sleep(Duration::from_millis(250));
    }
    event(
        dir,
        "boundary_observed",
        "all observed game processes and inherited handles have gone",
    )?;
    let marker = read_optional(&dir.join("fixture-save"))?
        .unwrap_or_else(|| "real game (no synthetic marker)".into());
    // Freeze the captured marker now. A late save during the simulated upload
    // must not retroactively appear to have existed at the observed boundary.
    let captured = crate::now_ms();
    thread::sleep(Duration::from_secs(3));
    let receipt = format!(
        "session={}\ncaptured_unix_ms={}\nobserver_completed_unix_ms={}\nfixture={}\n",
        dir.file_name().unwrap_or_default().to_string_lossy(),
        captured,
        crate::now_ms(),
        marker
    );
    atomic_write(dir, "receipt", &receipt)?;
    event(dir, "receipt_written", marker)
}
