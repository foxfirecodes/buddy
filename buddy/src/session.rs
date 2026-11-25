use crate::{
    files::{Cache, Files, Restore, SaveLocation},
    local::{Api, Local, OfflinePrep, Pending, Setup, unreachable},
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

    fn recover_after_game(
        self,
        title: &str,
        message: &str,
        retry: Option<&str>,
        error: anyhow::Error,
    ) -> Result<()> {
        let options: &[&str] = match retry {
            Some(action) => &["Later", action],
            None => &["Close"],
        };
        let choice = match self {
            Self::Window => {
                crate::ui::choose_with_details(title, message, options, &format!("{error:#}"))?
            }
            Self::Terminal => self.choose(title, &format!("{message}\n\n{error:#}"), options)?,
            Self::Unattended => return Err(error.context(format!("{title}. {message}"))),
        };
        ensure!(choice == 1, crate::ui::Cancelled);
        Ok(())
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
    fn commit(&mut self, tree: &Tree, close: bool, operation: String) -> Result<i64> {
        let progress =
            crate::ui::Progress::start(self.ui, &self.scope.title, "Checking missing chunks…");
        self.api
            .upload(self.cache, tree, |d, t| progress.uploaded(d, t))?;
        drop(progress);
        let full = self
            .head
            .as_ref()
            .map(|r| {
                // Changing an ignore alone does not generate a backup on the next exit.
                if self.scope.slots.values().any(|s| !s.ignores.is_empty())
                    && r.tree.project(&self.scope) == *tree
                {
                    r.tree.clone()
                } else {
                    r.tree.overlay(&self.scope, tree)
                }
            })
            .unwrap_or_else(|| tree.clone());
        let op = Action::Commit {
            lease: self.lease.clone(),
            expected: self.head.as_ref().map(|r| r.id),
            tree: full.clone(),
            close,
            op: operation,
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

    fn preserve_displaced_session(&self, pending: &Pending) -> Result<i64> {
        let progress =
            crate::ui::Progress::start(self.ui, &self.scope.title, "Preserving this session…");
        self.api
            .upload(self.cache, &pending.tree, |d, t| progress.uploaded(d, t))?;
        self.api.rpc(Action::Preserve {
            game: self.scope.game.clone(),
            slots: self.scope.slots.keys().cloned().collect(),
            local: pending.tree.clone(),
            time: pending.time,
            op: pending.op.clone(),
        })
    }

    fn publish_after_game(&mut self, setup: &Setup) -> Result<()> {
        let tree = setup.base.as_ref().context("missing captured state")?;
        // A missing response can mean this commit already closed the lease.
        let operation = id();
        loop {
            let mut error = match self.commit(tree, true, operation.clone()) {
                Ok(_) => return Ok(()),
                Err(error) => error,
            };
            if error
                .downcast_ref::<crate::local::ApiError>()
                .is_some_and(|e| e.0 == 412)
            {
                let Some(pending) = setup.pending.last() else {
                    return Ok(());
                };
                // Preserve the captured bytes without disturbing the current lease.
                match self.preserve_displaced_session(pending) {
                    Ok(_) => return self.ui.recover_after_game(
                        "Session saved as a backup",
                        "The server session changed. Your save has a permanent server backup and is queued on this device for reconciliation at the next sync.",
                        None,
                        error,
                    ),
                    Err(failure) => error = failure,
                }
            }
            let queued = !setup.pending.is_empty();
            let title = if queued {
                "Save queued on this device"
            } else {
                "Couldn’t close the sync session"
            };
            let status = error.downcast_ref::<crate::local::ApiError>().map(|e| e.0);
            if status == Some(401) {
                return self.ui.recover_after_game(
                    title,
                    "Sign in to this account again in Buddy, then choose Sync now for this game.",
                    None,
                    error,
                );
            }
            let message = if status == Some(507) {
                "The server is out of storage for this account. Free space or increase the account quota, then retry sync."
            } else if queued && unreachable(&error) {
                "Buddy couldn’t reach the server. Reconnect and retry, or leave this save for the next sync."
            } else if queued {
                "Sync didn’t finish. Retry now, or leave your saved changes queued for the next sync."
            } else {
                "The game files are unchanged. Retry to finish closing the session, or leave it for the next sync."
            };
            self.ui
                .recover_after_game(title, message, Some("Retry sync"), error)?;
        }
    }
    fn merge(&mut self, tree: &Tree, base: Option<&Tree>, time: i64) -> Result<Tree> {
        let Some(remote) = self.head.clone() else {
            self.commit(tree, false, id())?;
            return Ok(tree.clone());
        };
        let remote_tree = remote.tree.project(&self.scope);
        if remote_tree == *tree || base == Some(tree) || base.is_none() && !tree.populated() {
            return Ok(remote_tree);
        }
        if base == Some(&remote_tree) || base.is_none() && !remote_tree.populated() {
            self.commit(tree, false, id())?;
            return Ok(tree.clone());
        }
        let progress =
            crate::ui::Progress::start(self.ui, &self.scope.title, "Checking missing chunks…");
        self.api
            .upload(self.cache, tree, |d, t| progress.uploaded(d, t))?;
        drop(progress);
        let local: i64 = self.api.rpc(Action::Conflict {
            lease: self.lease.clone(),
            expected: remote.id,
            local: remote.tree.overlay(&self.scope, tree),
            time,
            op: id(),
        })?;
        let picked = if matches!(self.ui, Interface::Window) {
            let copy = |device: &str, time, tree: &Tree, backup| {
                let (files, bytes) = tree.file_totals();
                crate::ui::SaveCopy {
                    device: device.to_owned(),
                    time,
                    files,
                    bytes,
                    changes: tree.changes(base),
                    backup,
                }
            };
            crate::ui::choose_conflict(
                &self.scope.title,
                [
                    copy(&self.api.c.device, time, tree, local),
                    copy(&remote.origin, remote.time, &remote_tree, remote.id),
                ],
            )?
        } else {
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
            self.ui.choose(
                "Which save should Buddy continue with?",
                &details,
                &["Use this device", "Use server copy", "Decide later"],
            )?
        };
        match picked {
            0 => {
                self.commit(tree, false, id())?;
                Ok(tree.clone())
            }
            1 => Ok(remote_tree),
            _ => Err(anyhow::Error::new(crate::ui::Cancelled)
                .context("both backups preserved; decision pending")),
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
    #[cfg(unix)]
    let code = {
        use std::os::unix::process::ExitStatusExt;
        status
            .code()
            .unwrap_or_else(|| 128 + status.signal().unwrap_or(0))
    };
    #[cfg(not(unix))]
    let code = status.code().unwrap_or(1);
    if manual
        && ui
            .choose(
                "Finish session",
                "Close the game and any launcher that can still write its saves.",
                &["Game is closed", "Leave pending"],
            )
            .context(PostGame(code))?
            != 0
    {
        return Err(anyhow::Error::new(crate::ui::Cancelled)
            .context("session left pending")
            .context(PostGame(code)));
    }
    Ok(code)
}
fn finish_local(
    clean_exit: bool,
    local: &Local,
    c: &Credentials,
    s: &mut Setup,
    scope: &Recipe,
    cache: &Cache,
) -> Result<()> {
    let files = Files::open(scope, &s.bindings, &local.home)?;
    let tree = files.scan(&s.recipe, cache, None, clean_exit)?;
    let mut active_base = s.active_base.take();
    let changed = active_base.as_ref() != Some(&tree);
    if changed {
        s.pending.push(Pending {
            tree: tree.clone(),
            base: active_base.take(),
            time: now(),
            op: id(),
        });
    }
    let previous_base = s.base.replace(tree);
    s.active = false;
    if let Err(error) = local.save(c, s) {
        // A retry must start from the same active session as the durable record.
        s.base = previous_base;
        if changed {
            active_base = s.pending.pop().expect("just queued this capture").base;
        }
        s.active_base = active_base;
        s.active = true;
        return Err(error);
    }
    Ok(())
}

fn capture_after_game(
    local: &Local,
    c: &Credentials,
    s: &mut Setup,
    scope: &Recipe,
    cache: &Cache,
    ui: Interface,
) -> Result<()> {
    loop {
        let progress = crate::ui::Progress::start(ui, &s.recipe.title, "Capturing changed files…");
        let result = finish_local(true, local, c, s, scope, cache);
        drop(progress);
        let Err(error) = result else { return Ok(()) };
        let full = error.chain().any(|cause| {
            cause
                .downcast_ref::<std::io::Error>()
                .is_some_and(|io| io.kind() == std::io::ErrorKind::StorageFull)
                || cause
                    .downcast_ref::<rusqlite::Error>()
                    .is_some_and(|e| e.sqlite_error_code() == Some(rusqlite::ErrorCode::DiskFull))
        });
        let message = if full {
            format!(
                "Free space on the drive containing Buddy’s data folder, then retry capture.\n\n{}",
                local.home.display()
            )
        } else if let Some(location) = error.downcast_ref::<SaveLocation>() {
            location.to_string()
        } else {
            "Check your save files and Buddy’s data folder, then retry capture. Choosing Later leaves the session open for recovery.".into()
        };
        ui.recover_after_game(
            "Couldn’t capture game saves",
            &message,
            Some("Retry capture"),
            error,
        )?;
    }
}
fn recover_local(
    local: &Local,
    c: &Credentials,
    s: &mut Setup,
    scope: &Recipe,
    cache: &Cache,
    ui: Interface,
) -> Result<()> {
    if let Some(plan) = s.restore.clone() {
        let files = Files::open(scope, &s.bindings, &local.home)?;
        files.apply(scope, cache, &plan)?;
        if !plan.rollback {
            s.base = Some(plan.after.project(scope));
        }
        s.restore = None;
        local.save(c, s)?;
    }
    if s.active {
        ensure!(
            ui.choose(
                "Recover interrupted session",
                "Confirm the previous game is fully closed before capturing its surviving files.",
                &["Cancel", "Game is closed"]
            )? == 1,
            crate::ui::Cancelled
        );
        finish_local(false, local, c, s, scope, cache)?;
    }
    Ok(())
}
fn acquire(api: &Api, s: &Setup, ui: Interface, preparing: bool) -> Result<Acquisition> {
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
                ensure!(
                    !preparing,
                    "Still open on {}. Close the game there and prepare again.",
                    held.device
                );
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
                        ensure!(ui.choose("Confirm takeover",&format!("{} may still be playing. Its next save will need reconciliation.",held.device),&["Cancel","Confirm takeover"])?==1,crate::ui::Cancelled);
                        takeover = Some(held.generation);
                    }
                    _ => bail!(crate::ui::Cancelled),
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
    pub prepare_offline: bool,
    pub command: Vec<OsString>,
    pub manual: bool,
    pub restore: Option<i64>,
    pub pin: bool,
    pub expected_scope: Option<String>,
}
pub fn synchronize(local: &Local, g: &str, options: RunOptions, ui: Interface) -> Result<i32> {
    let _executable_guard = if options.command.is_empty() {
        None
    } else {
        crate::update::game_guard()?
    };
    let c = local.credentials()?;
    let account = local.scope(&c);
    ensure!(
        options
            .expected_scope
            .as_ref()
            .is_none_or(|s| s == &account),
        "account changed; reopen the game"
    );
    let api = Api::new(c.clone())?;
    let _game_lock = local.lock(&format!("{account}:{g}"))?;
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
    let _cache_lock = local.cache_lock(&c, false)?;
    let cache = local.cache(&c)?;
    if s.active && s.restore.is_none() {
        recover_local(local, &c, &mut s, &scope, &cache, ui)?;
    }
    if s.session.is_none() {
        s.session = Some(id());
        local.save(&c, &s)?;
    }
    let mut progress: Option<crate::ui::Progress> = None;
    let acquired = loop {
        match acquire(&api, &s, ui, options.prepare_offline) {
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
                if s.offline_prep.is_none() {
                    match ui.choose(
                        "Server unreachable",
                        &msg,
                        &["Retry", "Play offline", "Cancel"],
                    )? {
                        0 => continue,
                        1 => (),
                        _ => bail!(crate::ui::Cancelled),
                    }
                }
                recover_local(local, &c, &mut s, &scope, &cache, ui)?;
                let files = Files::open(&scope, &s.bindings, &local.home)?;
                let before = files.capture(&s.recipe, &cache, None)?;
                drop(files);
                if !before.populated() && s.base.as_ref().is_none_or(Tree::populated) {
                    ensure!(
                        ui.choose(
                            "Start with empty saves?",
                            "This device has no saved progress in the selected paths.",
                            &["Cancel", "Start a new game"]
                        )? == 1,
                        crate::ui::Cancelled
                    );
                }
                let prepared = s.offline_prep.take();
                if prepared.is_some() {
                    s.offline_prep = Some(OfflinePrep::Played);
                }
                s.active = true;
                s.active_base = s.base.clone();
                local.save(&c, &s)?;
                drop(progress.take());
                let mut started = false;
                let code = match execute(&options.command, options.manual, ui, &mut started) {
                    Ok(code) => code,
                    Err(error) => {
                        if !started {
                            s.offline_prep = prepared;
                            s.active = false;
                            s.active_base = None;
                            local.save(&c, &s)?;
                        }
                        return Err(error);
                    }
                };
                capture_after_game(local, &c, &mut s, &scope, &cache, ui)
                    .context(PostGame(code))?;
                return Ok(code);
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
        retired_slots,
    } = acquired
    else {
        unreachable!()
    };
    let beat = Heartbeat::new(api.clone(), lease.clone());
    let mut completed_code = None;
    let result = (|| -> Result<i32> {
        recipe.validate()?;
        let mut removed = scope.clone();
        removed.slots.retain(|k, _| !recipe.slots.contains_key(k));
        ensure!(
            removed.slots.keys().all(|k| retired_slots.contains(k)),
            "server recipe changed unexpectedly; reopen setup"
        );
        ensure!(
            scope
                .slots
                .iter()
                .all(|(k, slot)| recipe.slots.get(k).is_none_or(|r| r.kind == slot.kind)),
            "server slot changed type; reopen setup"
        );
        ensure!(
            removed.slots.is_empty() || s.restore.is_none(),
            "a location in the interrupted restore was removed from the recipe; use buddy rollback {} to recover the previous files before syncing",
            g
        );
        if s.restore.is_some() {
            // Persist newly excluded paths before replay so even an offline retry
            // respects them. Widen the managed scope only after replay succeeds.
            let mut narrowed = false;
            for (name, slot) in &mut scope.slots {
                let mut rules = slot.ignores.clone();
                rules.extend(recipe.slots[name].ignores.iter().cloned());
                if rules != slot.ignores {
                    slot.set_ignores(rules)?;
                    s.recipe.slots.insert(name.clone(), slot.clone());
                    narrowed = true;
                }
            }
            if narrowed {
                local.save(&c, &s)?;
            }
        }
        recover_local(local, &c, &mut s, &scope, &cache, ui)?;
        if !removed.slots.is_empty() {
            for p in &s.pending {
                let historical = p.tree.project(&removed);
                if p.base.as_ref().map(|t| t.project(&removed)).as_ref() != Some(&historical) {
                    api.upload(&cache, &p.tree, |_, _| {})?;
                    api.rpc::<i64>(Action::Preserve {
                        game: g.to_owned(),
                        slots: scope.slots.keys().cloned().collect(),
                        local: p.tree.clone(),
                        time: p.time,
                        op: p.op.clone(),
                    })?;
                }
            }
            scope.slots.retain(|k, _| recipe.slots.contains_key(k));
            for p in &mut s.pending {
                p.tree = p.tree.project(&scope);
                p.base = p.base.as_ref().map(|t| t.project(&scope));
                p.op = id();
            }
        }
        if s.revision != revision
            || s.recipe != recipe
            || s.shared != shared
            || s.retired_slots != retired_slots
        {
            crate::setup::accept_recipe(
                &mut s,
                GameInfo {
                    recipe,
                    revision,
                    shared,
                    retired_slots,
                },
            );
            scope = s.recipe.clone();
            scope.slots.retain(|k, _| s.bindings.contains_key(k));
            for p in &mut s.pending {
                p.tree = p.tree.project(&scope);
                p.base = p.base.as_ref().map(|b| b.project(&scope));
            }
            if scope.slots.is_empty() {
                s.pending.clear();
            }
            local.save(&c, &s)?;
        }
        if scope.slots.is_empty() {
            ensure!(
                options.command.is_empty()
                    && options.restore.is_none()
                    && !options.pin
                    && !options.prepare_offline,
                "all selected locations were removed from the recipe; choose a location before continuing"
            );
            api.rpc::<()>(Action::Release {
                lease: lease.clone(),
            })?;
            return Ok(0);
        }
        let files = Files::open(&scope, &s.bindings, &local.home)?;
        if let Some(id) = options.restore
            && matches!(ui, Interface::Window)
        {
            // These approved device bindings now hold the same locks used for restore.
            let targets = scope
                .slots
                .keys()
                .map(|name| s.bindings[name].path.display().to_string())
                .collect::<Vec<_>>()
                .join("\n");
            ensure!(
                ui.choose(
                    "Restore backup",
                    &format!(
                        "Restore backup #{id} to the server and these locations?\n\n{targets}"
                    ),
                    &["Cancel", "Restore"]
                )? == 1,
                crate::ui::Cancelled
            );
        }
        let mut online = Online {
            api: &api,
            lease: lease.clone(),
            head,
            ui,
            cache: &cache,
            scope: scope.clone(),
        };
        // Queue entries contain immutable bytes, so choosing a remote winner cannot erase later offline sessions.
        progress = Some(crate::ui::Progress::start(
            ui,
            &s.recipe.title,
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
                &s.recipe.title,
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
                    p.downloaded(d, t)
                }
            })?;
            if let Some(p) = &progress {
                p.status("Verifying game files…");
            }
            if restoring_backup {
                online.commit(&next, false, id())?;
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
            if options.prepare_offline || matches!(s.offline_prep, Some(OfflinePrep::Ready { .. }))
            {
                let revision = online
                    .head
                    .as_ref()
                    .context("No synced backup to prepare")?;
                s.offline_prep = Some(OfflinePrep::ready(revision));
            } else if matches!(s.offline_prep, Some(OfflinePrep::Played)) {
                s.offline_prep = None;
            }
            return Ok(0);
        }
        api.rpc::<()>(Action::Renew {
            lease: lease.clone(),
        })?;
        drop(files);
        drop(progress.take());
        if matches!(s.offline_prep, Some(OfflinePrep::Played)) {
            s.offline_prep = None;
        }
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
        capture_after_game(local, &c, &mut s, &scope, &cache, ui)?;
        online.publish_after_game(&s)?;
        if matches!(s.offline_prep, Some(OfflinePrep::Ready { .. })) {
            s.offline_prep = online.head.as_ref().map(OfflinePrep::ready);
        }
        s.pending.clear();
        Ok(code)
    })();
    drop(beat);
    let finalized = (|| match result {
        Ok(code) => {
            s.session = None;
            while let Err(error) = local.save(&c, &s) {
                if completed_code.is_none() {
                    return Err(error);
                }
                ui.recover_after_game(
                    "Couldn’t update local sync status",
                    "This session has no changes left to upload. Retry saving Buddy’s sync status or choose Sync now later.",
                    Some("Retry saving status"),
                    error,
                )?;
            }
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

pub fn prepare_games(
    local: &Local,
    games: &[String],
    scope: &str,
    mut report: impl FnMut(&str, Result<String>),
) -> Result<()> {
    let c = local.credentials()?;
    ensure!(
        local.scope(&c) == scope,
        "Account changed. Reopen offline preparation."
    );
    local.write(&format!("offline-picks/{scope}"), &games)?;
    for game in games {
        let result = (|| {
            synchronize(
                local,
                game,
                RunOptions {
                    prepare_offline: true,
                    expected_scope: Some(scope.to_owned()),
                    ..crate::options()
                },
                Interface::Unattended,
            )?;
            match local.setup(&c, game)?.offline_prep {
                Some(ready @ OfflinePrep::Ready { .. }) => Ok(ready.to_string()),
                _ => bail!("Setup changed. Prepare this game again."),
            }
        })();
        report(game, result);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::files::Binding;
    use std::fs;

    #[test]
    fn failed_local_capture_commit_can_be_retried_once() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let local = Local::open(Some(temp.path().join("profile")))?;
        let creds = Credentials {
            url: "http://localhost".into(),
            account: "player".into(),
            device: "test".into(),
            token: "test".into(),
            paired_ca: None,
        };
        let recipe: Recipe = serde_json::from_value(serde_json::json!({
            "game": "sample", "title": "Sample", "retention": 3, "slots": {"save": {"kind":"File"}}
        }))?;
        let mut setup = Setup::new(GameInfo {
            recipe: recipe.clone(),
            revision: 1,
            shared: false,
            retired_slots: Default::default(),
        });
        let path = temp.path().join("save.dat");
        fs::write(&path, "before")?;
        setup
            .bindings
            .insert("save".into(), Binding::new(&path, &Kind::File)?);
        let cache = local.cache(&creds)?;
        let before =
            Files::open(&recipe, &setup.bindings, &local.home)?.capture(&recipe, &cache, None)?;
        setup.base = Some(before.clone());
        setup.active_base = Some(before.clone());
        setup.active = true;
        local.save(&creds, &setup)?;
        fs::write(path, "after")?;

        let db = rusqlite::Connection::open(local.home.join("client.sqlite3"))?;
        db.execute_batch("CREATE TRIGGER refuse_capture BEFORE UPDATE ON state BEGIN SELECT RAISE(ABORT, 'test write failure'); END")?;
        assert!(finish_local(true, &local, &creds, &mut setup, &recipe, &cache).is_err());
        for state in [&setup, &local.setup(&creds, "sample")?] {
            assert!(state.active && state.pending.is_empty());
            assert_eq!(state.active_base.as_ref(), Some(&before));
            assert_eq!(state.base.as_ref(), Some(&before));
        }
        db.execute_batch("DROP TRIGGER refuse_capture")?;
        finish_local(true, &local, &creds, &mut setup, &recipe, &cache)?;
        let captured = local.setup(&creds, "sample")?;
        assert!(!captured.active && captured.active_base.is_none());
        assert_eq!(captured.pending.len(), 1);
        assert_eq!(captured.pending[0].base.as_ref(), Some(&before));
        assert_eq!(captured.base.as_ref(), Some(&captured.pending[0].tree));
        Ok(())
    }
}
