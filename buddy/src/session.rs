use crate::{
    files::{Cache, Files, Restore},
    local::{Api, Local, Pending, Setup, unreachable},
    model::*,
};
use anyhow::{Context, Result, bail, ensure};
use std::{ffi::OsString, process::Command, sync::mpsc, thread, time::Duration};

#[derive(Clone, Copy)]
pub enum Interface {
    Window,
    Terminal,
    Unattended,
}
#[derive(Debug)]
pub struct PostGame(pub i32);
impl std::fmt::Display for PostGame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "The game exited with code {}. Sync needs attention before the next session",
            self.0
        )
    }
}
impl std::error::Error for PostGame {}
impl Interface {
    pub fn choose(self, title: &str, message: &str, options: &[&str]) -> Result<usize> {
        ensure!((1..=3).contains(&options.len()), "invalid choice");
        match self {
            Self::Unattended => bail!("decision required: {title}. {message}"),
            Self::Terminal => {
                let text = format!("{title}\n{message}");
                let result = inquire::Select::new(&text, options.to_vec()).prompt()?;
                Ok(options.iter().position(|v| *v == result).unwrap())
            }
            Self::Window => crate::ui::choose(title, message, options),
        }
    }
}
struct Heartbeat {
    stop: Option<mpsc::Sender<()>>,
    thread: Option<thread::JoinHandle<()>>,
}
impl Heartbeat {
    fn new(api: Api, lease: Lease) -> Self {
        let (tx, rx) = mpsc::channel();
        let thread = thread::spawn(move || {
            while matches!(
                rx.recv_timeout(Duration::from_secs(25)),
                Err(mpsc::RecvTimeoutError::Timeout)
            ) {
                if let Err(e) = api.rpc::<()>(Action::Renew {
                    lease: lease.clone(),
                }) {
                    eprintln!("Lease renewal: {e}");
                }
            }
        });
        Self {
            stop: Some(tx),
            thread: Some(thread),
        }
    }
}
impl Drop for Heartbeat {
    fn drop(&mut self) {
        self.stop.take();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}
struct Online<'a> {
    api: &'a Api,
    lease: Lease,
    head: Option<Revision>,
    ui: Interface,
    cache: &'a Cache,
    scope: Recipe,
}
impl Online<'_> {
    fn commit(&mut self, tree: &Tree, close: bool) -> Result<i64> {
        let progress = crate::ui::Progress::start(self.ui, "Checking missing chunks…");
        self.api.upload(self.cache, tree, |d, t| {
            progress.transfer("Uploading", d, t)
        })?;
        drop(progress);
        let full = self
            .head
            .as_ref()
            .map(|r| r.tree.overlay(&self.scope, tree))
            .unwrap_or_else(|| tree.clone());
        let op = Action::Commit {
            lease: self.lease.clone(),
            expected: self.head.as_ref().map(|r| r.id),
            tree: full.clone(),
            close,
            op: id(),
        };
        let id = self.api.rpc::<i64>(op)?;
        if self.head.as_ref().is_none_or(|r| r.id != id) {
            self.head = Some(Revision {
                id,
                tree: full,
                origin: self.api.c.device.clone(),
                time: now(),
            });
        }
        Ok(id)
    }
    fn merge(&mut self, tree: &Tree, base: Option<&Tree>, time: i64) -> Result<Tree> {
        let Some(remote) = self.head.clone() else {
            self.commit(tree, false)?;
            return Ok(tree.clone());
        };
        let remote_tree = remote.tree.project(&self.scope);
        if remote_tree == *tree || base == Some(tree) || base.is_none() && !tree.populated() {
            return Ok(remote_tree);
        }
        if base == Some(&remote_tree) || base.is_none() && !remote_tree.populated() {
            self.commit(tree, false)?;
            return Ok(tree.clone());
        }
        let progress = crate::ui::Progress::start(self.ui, "Checking missing chunks…");
        self.api.upload(self.cache, tree, |d, t| {
            progress.transfer("Uploading", d, t)
        })?;
        drop(progress);
        let local: i64 = self.api.rpc(Action::Conflict {
            lease: self.lease.clone(),
            expected: remote.id,
            local: remote.tree.overlay(&self.scope, tree),
            time,
            op: id(),
        })?;
        let details = format!(
            "Both versions have permanent backups.\n\nThis device: {} · captured {} · {} (backup #{local})\n{}\n\nServer: {} · committed {} · {} (backup #{})\n{}",
            self.api.c.device,
            when(time),
            tree.summary(),
            tree.changes(base),
            remote.origin,
            when(remote.time),
            remote_tree.summary(),
            remote.id,
            remote_tree.changes(base)
        );
        match self.ui.choose(
            "Which save should Buddy continue with?",
            &details,
            &["Use this device", "Use server copy", "Decide later"],
        )? {
            0 => {
                self.commit(tree, false)?;
                Ok(tree.clone())
            }
            1 => Ok(remote_tree),
            _ => bail!("both backups preserved; decision pending"),
        }
    }
}
fn execute(command: &[OsString], manual: bool, ui: Interface, started: &mut bool) -> Result<i32> {
    let (exe, args) = command.split_first().context("missing game command")?;
    let mut child = Command::new(exe);
    child
        .args(args)
        .env_remove("BUDDY_PASSWORD")
        .env_remove("BUDDY_INVITE");
    let status = if manual {
        let mut process = child.spawn().context("launch game")?;
        *started = true;
        process.wait()?
    } else {
        buddy_process::wait_for_game(child, || *started = true).context("launch or observe game")?
    };
    if manual {
        ensure!(
            ui.choose(
                "Finish session",
                "Close the game and any launcher that can still write its saves.",
                &["Game is closed", "Leave pending"]
            )? == 0,
            "session left pending"
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        Ok(status
            .code()
            .unwrap_or_else(|| 128 + status.signal().unwrap_or(0)))
    }
    #[cfg(not(unix))]
    {
        Ok(status.code().unwrap_or(1))
    }
}
fn finish_local(
    clean_exit: bool,
    local: &Local,
    c: &Credentials,
    s: &mut Setup,
    files: &Files,
    cache: &Cache,
) -> Result<()> {
    let tree = files.scan(&s.recipe, cache, None, clean_exit)?;
    if s.active_base.as_ref() != Some(&tree) {
        s.pending.push(Pending {
            tree: tree.clone(),
            base: s.active_base.clone(),
            time: now(),
            op: id(),
        });
    }
    s.base = Some(tree);
    s.active = false;
    s.active_base = None;
    local.save(c, s)
}
fn acquire(api: &Api, s: &Setup, ui: Interface) -> Result<Acquisition> {
    let session = s.session.clone().context("missing session id")?;
    let mut takeover = None;
    loop {
        let a = api.rpc(Action::Acquire {
            game: s.recipe.game.clone(),
            session: session.clone(),
            takeover: takeover.take(),
        })?;
        match a {
            Acquisition::Granted { .. } => return Ok(a),
            Acquisition::Busy(held) => {
                let description = format!(
                    "{} opened this game at {}. Last lease expiry {}.",
                    held.device,
                    when(held.since),
                    when(held.expires)
                );
                match ui.choose(
                    "Another device has this game open",
                    &description,
                    &["Wait and retry", "Take over…", "Cancel"],
                )? {
                    0 => thread::sleep(Duration::from_secs(2)),
                    1 => {
                        ensure!(ui.choose("Confirm takeover",&format!("{} may still be playing. Its next save will need reconciliation.",held.device),&["Cancel","Confirm takeover"])?==1,"cancelled");
                        takeover = Some(held.generation);
                    }
                    _ => bail!("cancelled"),
                }
            }
        }
    }
}
pub fn rollback(local: &Local, g: &str) -> Result<()> {
    let c = local.credentials()?;
    let _game = local.lock(&format!("{}:{g}", local.scope(&c)))?;
    let mut s = local.setup(&c, g)?;
    ensure!(!s.active, "close the game first");
    let _paths = local.lock_paths(s.bindings.values().map(|b| b.path.as_path()))?;
    let _cache_lock = local.cache_lock(&c, false)?;
    let cache = local.cache(&c)?;
    let mut scope = s.recipe.clone();
    scope.slots.retain(|k, _| s.bindings.contains_key(k));
    let files = Files::open(&scope, &s.bindings, &local.home)?;
    let mut plan = s.restore.clone().context("no interrupted restore")?;
    if !plan.rollback {
        std::mem::swap(&mut plan.before, &mut plan.after);
        plan.rollback = true;
        s.restore = Some(plan.clone());
        local.save(&c, &s)?;
    }
    files.apply(&scope, &cache, &plan)?;
    s.restore = None;
    local.save(&c, &s)
}
pub struct RunOptions {
    pub command: Vec<OsString>,
    pub manual: bool,
    pub restore: Option<i64>,
    pub pin: bool,
}
pub fn synchronize(local: &Local, g: &str, options: RunOptions, ui: Interface) -> Result<i32> {
    let c = local.credentials()?;
    let api = Api::new(c.clone())?;
    let _game_lock = local.lock(&format!("{}:{g}", local.scope(&c)))?;
    #[cfg(target_os = "linux")]
    let mut s = crate::proton::first_launch(
        local,
        &api,
        g,
        &options.command,
        ui,
        local.maybe_setup(&c, g)?,
    )?;
    #[cfg(not(target_os = "linux"))]
    let mut s = local.setup(&c, g)?;
    if s.draft.is_some()
        && crate::setup::idle(&s)
        && let Err(e) = crate::setup::publish_draft(local, &api, &mut s, ui)
    {
        // An existing holder blocks recipe writes. Keep the draft and enter the
        // normal lease flow with the already approved selections.
        let held = !s.bindings.is_empty()
            && e.downcast_ref::<crate::local::ApiError>()
                .is_some_and(|v| v.0 == 412);
        let offline_launch = !options.command.is_empty() && unreachable(&e);
        if !held && !offline_launch {
            return Err(e);
        }
    }
    // Acquire shared ancestor locks and exclusive slot locks in canonical order.
    let _paths = local.lock_paths(s.bindings.values().map(|b| b.path.as_path()))?;
    let mut scope = s.recipe.clone();
    scope.slots.retain(|k, _| s.bindings.contains_key(k));
    ensure!(
        !scope.slots.is_empty(),
        "choose at least one file or directory for this device"
    );
    let files = Files::open(&scope, &s.bindings, &local.home)?;
    let _cache_lock = local.cache_lock(&c, false)?;
    let cache = local.cache(&c)?;
    if let Some(plan) = s.restore.clone() {
        files.apply(&s.recipe, &cache, &plan)?;
        if !plan.rollback {
            s.base = Some(plan.after);
        }
        s.restore = None;
        local.save(&c, &s)?;
    }
    if s.active {
        ensure!(
            ui.choose(
                "Recover interrupted session",
                "Confirm the previous game is fully closed before capturing its surviving files.",
                &["Cancel", "Game is closed"]
            )? == 1,
            "cancelled"
        );
        finish_local(false, local, &c, &mut s, &files, &cache)?;
    }
    if s.session.is_none() {
        s.session = Some(id());
        local.save(&c, &s)?;
    }
    let mut progress: Option<crate::ui::Progress> = None;
    let acquired = loop {
        match acquire(&api, &s, ui) {
            Ok(grant) => break grant,
            Err(e) if !options.command.is_empty() && unreachable(&e) => {
                ensure!(
                    !s.needs_first_pull,
                    "Connect to your Buddy server to finish the first Proton restore before playing."
                );
                let msg = format!(
                    "Cannot reach {}. Use the last approved setup and keep this session on {} until it reconnects?",
                    c.url, c.device
                );
                match ui.choose(
                    "Server unreachable",
                    &msg,
                    &["Retry", "Play offline", "Cancel"],
                )? {
                    0 => continue,
                    1 => {
                        let before = files.capture(&s.recipe, &cache, None)?;
                        if !before.populated() && s.base.as_ref().is_none_or(Tree::populated) {
                            ensure!(
                                ui.choose(
                                    "Start with empty saves?",
                                    "This device has no saved progress in the selected paths.",
                                    &["Cancel", "Start a new game"]
                                )? == 1,
                                "cancelled"
                            );
                        }
                        s.active = true;
                        s.active_base = s.base.clone();
                        local.save(&c, &s)?;
                        drop(progress.take());
                        let mut started = false;
                        let code = match execute(&options.command, options.manual, ui, &mut started)
                        {
                            Ok(code) => code,
                            Err(error) => {
                                if !started {
                                    s.active = false;
                                    s.active_base = None;
                                    local.save(&c, &s)?;
                                }
                                return Err(error);
                            }
                        };
                        let _capture =
                            crate::ui::Progress::start(ui, "Preserving offline progress…");
                        finish_local(true, local, &c, &mut s, &files, &cache)
                            .context(PostGame(code))?;
                        return Ok(code);
                    }
                    _ => bail!("cancelled"),
                }
            }
            Err(e) => return Err(e),
        }
    };
    let Acquisition::Granted {
        lease,
        head,
        recipe,
        revision,
        shared,
    } = acquired
    else {
        unreachable!()
    };
    let beat = Heartbeat::new(api.clone(), lease.clone());
    let mut online = Online {
        api: &api,
        lease: lease.clone(),
        head,
        ui,
        cache: &cache,
        scope: scope.clone(),
    };
    let mut completed_code = None;
    let result = (|| -> Result<i32> {
        recipe.validate()?;
        ensure!(
            scope
                .slots
                .iter()
                .all(|(k, slot)| recipe.slots.get(k).is_some_and(|r| r.kind == slot.kind)),
            "server recipe changed; review and bind its slots on this device"
        );
        s.recipe = recipe;
        s.revision = revision;
        s.shared = shared;
        // Queue entries contain immutable bytes, so choosing a remote winner cannot erase later offline sessions.
        progress = Some(crate::ui::Progress::start(
            ui,
            "Checking local saves and pending sessions…",
        ));
        let restore_target = options
            .restore
            .map(|id| api.rpc::<Revision>(Action::ReadRevision { game: g.into(), id }))
            .transpose()?;
        let live = files.capture(&s.recipe, &cache, None)?;
        drop(progress.take());
        let live_base = s.base.clone();
        while let Some(pending) = s.pending.first().cloned() {
            online.merge(&pending.tree, pending.base.as_ref(), pending.time)?;
            s.pending.remove(0);
            local.save(&c, &s)?;
        }
        let mut next = online.merge(&live, live_base.as_ref(), now())?;
        let restoring_backup = restore_target.is_some();
        if let Some(version) = restore_target {
            next = version.tree.project(&scope);
        }
        if restoring_backup || next != live {
            progress = Some(crate::ui::Progress::start(
                ui,
                if restoring_backup {
                    "Downloading and verifying the selected backup…"
                } else {
                    "Downloading and verifying changed files…"
                },
            ));
            next.validate(&scope)?;
            let mut chunks = next.chunks_since((!restoring_backup).then_some(&live))?;
            files.prepare_rollback(&live, &next, &cache, &mut chunks)?;
            api.download(&cache, chunks, |d, t| {
                if let Some(p) = &progress {
                    p.transfer("Downloading", d, t)
                }
            })?;
            if restoring_backup {
                online.commit(&next, false)?;
            }
        }
        if next != live {
            let plan = Restore {
                rollback: false,
                op: id(),
                before: live,
                after: next.clone(),
            };
            api.rpc::<()>(Action::Renew {
                lease: lease.clone(),
            })?;
            s.restore = Some(plan.clone());
            local.save(&c, &s)?;
            files.apply_new(&s.recipe, &cache, &plan)?;
            s.restore = None;
        }
        s.base = Some(next.clone());
        s.needs_first_pull = false;
        local.save(&c, &s)?;
        if options.pin {
            api.rpc::<()>(Action::Pin {
                game: g.into(),
                id: online.head.as_ref().context("no snapshot")?.id,
            })?;
        }
        if options.command.is_empty() {
            api.rpc::<()>(Action::Release {
                lease: lease.clone(),
            })?;
            return Ok(0);
        }
        api.rpc::<()>(Action::Renew {
            lease: lease.clone(),
        })?;
        drop(progress.take());
        s.active = true;
        s.active_base = Some(next);
        local.save(&c, &s)?;
        let mut started = false;
        let code = match execute(&options.command, options.manual, ui, &mut started) {
            Ok(code) => code,
            Err(error) => {
                if !started {
                    s.active = false;
                    s.active_base = None;
                    local.save(&c, &s)?;
                }
                return Err(error);
            }
        };
        completed_code = Some(code);
        {
            let _progress = crate::ui::Progress::start(ui, "Capturing changed files…");
            finish_local(true, local, &c, &mut s, &files, &cache)?;
        }
        let tree = s.base.clone().context("missing captured state")?;
        if let Err(e) = online.commit(&tree, true) {
            if e.downcast_ref::<crate::local::ApiError>()
                .is_some_and(|e| e.0 == 412)
                && let Some(p) = s.pending.last()
            {
                // The other player keeps the lease. Both versions become durable backups immediately.
                let progress = crate::ui::Progress::start(ui, "Preserving this session…");
                api.upload(&cache, &p.tree, |d, t| {
                    progress.transfer("Preserving backup", d, t)
                })?;
                api.rpc::<i64>(Action::Preserve {
                    game: g.into(),
                    slots: scope.slots.keys().cloned().collect(),
                    local: p.tree.clone(),
                    time: p.time,
                    op: p.op.clone(),
                })?;
            }
            return Err(e);
        }
        s.pending.clear();
        Ok(code)
    })();
    drop(beat);
    let finalized = (|| match result {
        Ok(code) => {
            s.session = None;
            local.save(&c, &s)?;
            Ok(code)
        }
        Err(e) => {
            if !s.active && api.rpc::<()>(Action::Release { lease }).is_ok() {
                s.session = None;
                local.save(&c, &s)?;
            }
            Err(e)
        }
    })();
    if let Some(code) = completed_code {
        finalized.context(PostGame(code))
    } else {
        finalized
    }
}
