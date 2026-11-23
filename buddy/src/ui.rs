use crate::{
    files::Binding,
    local::{Api, Launcher, Local, default_device, roots},
    model::*,
    options,
    session::{Interface, RunOptions, synchronize},
    setup::{self, Row, Selection},
};
use anyhow::{Context, Result, anyhow, ensure};
use eframe::egui::{self, Color32, RichText};
use serde::{Deserialize, Serialize};
use std::{
    io::{self, BufRead, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

fn native(size: [f32; 2]) -> eframe::NativeOptions {
    eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size(size),
        ..Default::default()
    }
}
fn style(ctx: &egui::Context) {
    ctx.all_styles_mut(|s| {
        s.visuals.override_text_color = Some(if s.visuals.dark_mode {
            Color32::from_rgb(229, 231, 235)
        } else {
            Color32::from_rgb(31, 41, 55)
        });
        s.text_styles
            .insert(egui::TextStyle::Body, egui::FontId::proportional(16.));
        s.text_styles
            .insert(egui::TextStyle::Button, egui::FontId::proportional(16.));
        s.text_styles
            .insert(egui::TextStyle::Heading, egui::FontId::proportional(24.));
        s.spacing.button_padding = egui::vec2(12., 7.);
        s.spacing.item_spacing = egui::vec2(8., 8.);
        s.spacing.interact_size.y = 32.;
    });
}
#[derive(Serialize, Deserialize)]
struct Question {
    title: String,
    message: String,
    choices: Vec<String>,
}
pub fn choose(title: &str, message: &str, options: &[&str]) -> Result<usize> {
    let mut child = Command::new(std::env::current_exe()?)
        .arg("_decision")
        .stdin(Stdio::piped())
        .spawn()?;
    serde_json::to_writer(
        child.stdin.take().context("open decision window")?,
        &Question {
            title: title.into(),
            message: message.into(),
            choices: options.iter().map(|s| s.to_string()).collect(),
        },
    )?;
    let code = child.wait()?.code().unwrap_or(1);
    ensure!(code >= 10 && code < 10 + options.len() as i32, "cancelled");
    Ok((code - 10) as usize)
}
pub fn decision_window() -> Result<i32> {
    let q: Question = serde_json::from_reader(io::stdin().lock())?;
    let result = Arc::new(AtomicUsize::new(1));
    let choice = result.clone();
    let title = q.title.clone();
    let mut focus = q
        .choices
        .iter()
        .position(|c| matches!(c.as_str(), "Cancel" | "Decide later" | "Leave pending"))
        .unwrap_or(0);
    let mut initial = true;
    eframe::run_simple_native(&title, native([690., 350.]), move |ctx, _| {
        if initial {
            style(ctx);
            initial = false;
        }
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.add_space(12.);
            ui.heading(&q.title);
            ui.add_space(12.);
            ui.label(&q.message);
            ui.add_space(18.);
            if ui.input(|i| i.key_pressed(egui::Key::ArrowLeft)) {
                focus = (focus + q.choices.len() - 1) % q.choices.len();
            }
            if ui.input(|i| i.key_pressed(egui::Key::ArrowRight)) {
                focus = (focus + 1) % q.choices.len();
            }
            let activate = ui.input(|i| i.key_pressed(egui::Key::Enter));
            ui.horizontal_wrapped(|ui| {
                for (i, opt) in q.choices.iter().enumerate() {
                    if ui.selectable_label(i == focus, opt).clicked() || activate && i == focus {
                        choice.store(i + 10, Ordering::Relaxed);
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                }
            });
            if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        });
    })
    .map_err(|e| anyhow!(e.to_string()))?;
    Ok(result.load(Ordering::Relaxed) as i32)
}
pub fn progress_window() -> Result<()> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        for line in io::stdin()
            .lock()
            .lines()
            .map_while(std::result::Result::ok)
        {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let mut text = String::from("Checking game files…");
    eframe::run_simple_native("Buddy sync", native([410., 115.]), move |ctx, _| {
        loop {
            match rx.try_recv() {
                Ok(s) => text = s,
                Err(mpsc::TryRecvError::Empty) => break,
                Err(_) => {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    break;
                }
            }
        }
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.add_space(15.);
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(&text);
            });
        });
        ctx.request_repaint_after(Duration::from_millis(150));
    })
    .map_err(|e| anyhow!(e.to_string()))
}
// A pipe ties the graphical indicator to this process even when Steam terminates the wrapper.
pub struct Progress(Option<mpsc::Sender<String>>);
impl Progress {
    pub fn start(ui: Interface, message: &str) -> Self {
        if !matches!(ui, Interface::Window) {
            return Self(None);
        }
        let (tx, rx) = mpsc::channel::<String>();
        let mut text = message.to_owned();
        thread::spawn(move || {
            let deadline = std::time::Instant::now() + Duration::from_secs(1);
            loop {
                match rx.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
                {
                    Ok(next) => text = next,
                    Err(mpsc::RecvTimeoutError::Disconnected) => return,
                    Err(mpsc::RecvTimeoutError::Timeout) => break,
                }
            }
            let Ok(binary) = std::env::current_exe() else {
                return;
            };
            let Ok(mut child) = Command::new(binary)
                .arg("_progress")
                .stdin(Stdio::piped())
                .spawn()
            else {
                return;
            };
            if let Some(mut pipe) = child.stdin.take() {
                let _ = writeln!(pipe, "{text}");
                for next in rx {
                    if writeln!(pipe, "{next}").is_err() {
                        break;
                    }
                }
            }
            let _ = child.wait();
        });
        Self(Some(tx))
    }
    pub fn transfer(&self, verb: &str, done: u64, total: u64) {
        if total > 0 {
            self.status(&format!(
                "{verb} {:.1} / {:.1} MiB",
                done as f64 / 1048576.,
                total as f64 / 1048576.
            ));
        }
    }
    pub fn status(&self, text: &str) {
        if let Some(tx) = &self.0 {
            let _ = tx.send(text.into());
        }
    }
}

type Suggestions = Vec<(String, Kind, Option<PathBuf>)>;
struct Detail {
    candidates: Vec<crate::manifest::Suggestion>,
    game: String,
    slots: Suggestions,
    bindings: Vec<(String, Binding)>,
    backups: Vec<Backup>,
    shared: bool,
    retention: u32,
    roots: Vec<(String, PathBuf)>,
    launcher: Option<Launcher>,
    offline: bool,
    draft: bool,
    shared_options: Vec<(String, String)>,
}
enum Event {
    Signed(Vec<Row>),
    Games(Vec<Row>),
    Detail(Box<Detail>),
    Done(String),
}
type Reply = Result<Event, String>;
struct App {
    local: Local,
    rx: mpsc::Receiver<Reply>,
    tx: mpsc::Sender<Reply>,
    busy: bool,
    signed: bool,
    url: String,
    account: String,
    password: String,
    device: String,
    invite: String,
    register: bool,
    private_file: bool,
    games: Vec<Row>,
    filter: String,
    server_only: bool,
    selected: String,
    title: String,
    slot: String,
    path: String,
    file: bool,
    settings: bool,
    source: Option<String>,
    root_name: String,
    launcher: Launcher,
    launcher_args: String,
    detail: Option<Detail>,
    note: String,
    error: bool,
}
impl App {
    fn new(local: Local) -> Self {
        let (tx, rx) = mpsc::channel();
        let c = local.credentials().ok();
        let signed = c.is_some();
        let mut app = Self {
            local,
            tx,
            rx,
            busy: false,
            signed,
            url: c
                .as_ref()
                .map(|c| c.url.clone())
                .unwrap_or_else(|| "http://127.0.0.1:7341".into()),
            account: c.as_ref().map(|c| c.account.clone()).unwrap_or_default(),
            password: String::new(),
            device: c.map(|c| c.device).unwrap_or_else(default_device),
            invite: String::new(),
            register: false,
            private_file: false,
            games: vec![],
            filter: String::new(),
            server_only: false,
            selected: String::new(),
            title: String::new(),
            slot: "saves".into(),
            path: String::new(),
            file: false,
            settings: false,
            source: None,
            root_name: String::new(),
            launcher: Launcher::default(),
            launcher_args: String::new(),
            detail: None,
            note: String::new(),
            error: false,
        };
        if signed {
            app.refresh();
        }
        app
    }
    fn job(&mut self, f: impl FnOnce(Local) -> Result<Event> + Send + 'static) {
        if self.busy {
            return;
        }
        self.busy = true;
        self.error = false;
        self.note.clear();
        let tx = self.tx.clone();
        let local = self.local.clone();
        thread::spawn(move || {
            let _ = tx.send(f(local).map_err(|e| format!("{e:#}")));
        });
    }
    fn refresh(&mut self) {
        self.job(|l| Ok(Event::Games(setup::games(&l)?)));
    }
    fn load(&mut self) {
        let game = self.selected.clone();
        let show_shared = self
            .games
            .iter()
            .any(|g| g.id == game && g.shared && !g.mine && !g.local);
        self.job(move |l| {
            let c = l.credentials()?;
            let s = l.setups(&c)?.into_iter().find(|s| s.recipe.game == game);
            let slots = setup::suggest(&l, &game)?;
            let (backups, offline) = if s.is_some() {
                match Api::new(c.clone())?.rpc(Action::Backups { game: game.clone() }) {
                    Ok(rows) => (rows, false),
                    Err(e) if crate::local::unreachable(&e) => (vec![], true),
                    Err(e) => return Err(e),
                }
            } else {
                (vec![], false)
            };
            let mut available = roots(&game);
            available.extend(l.named_roots(&c, &game)?);
            let mut shared_options = vec![];
            if show_shared {
                let catalog: Vec<GameInfo> = Api::new(c.clone())?.rpc(Action::Catalog)?;
                for i in catalog.into_iter().filter(|i| i.recipe.game == game) {
                    let hash = setup::recipe_hash(&i.recipe)?;
                    let names = i
                        .recipe
                        .slots
                        .keys()
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ");
                    shared_options.push((
                        format!("{} · {} · {}", i.recipe.title, names, &hash[..8]),
                        hash,
                    ));
                }
            }
            let d = Detail {
                shared_options,
                candidates: crate::manifest::candidates(&l, &game)?,
                roots: available.into_iter().collect(),
                launcher: l.launcher(&c, &game)?,
                offline,
                draft: s.as_ref().is_some_and(|s| s.draft.is_some()),
                game,
                slots,
                backups,
                shared: s.as_ref().is_some_and(|s| s.shared),
                retention: s.as_ref().map_or(3, |s| s.recipe.retention),
                bindings: s
                    .map(|s| s.bindings.into_iter().collect())
                    .unwrap_or_default(),
            };
            Ok(Event::Detail(Box::new(d)))
        });
    }
    fn select(&mut self, game: String, title: String) {
        self.selected = game;
        self.title = title;
        self.path.clear();
        self.source = None;
        self.slot = "saves".into();
        self.detail = None;
        self.load();
    }
    fn login(&mut self, ui: &mut egui::Ui) {
        ui.set_max_width(470.);
        ui.heading("Connect to Buddy");
        ui.add_space(12.);
        field(ui, "Server URL", &mut self.url);
        field(ui, "Account", &mut self.account);
        ui.label("Password");
        ui.add(egui::TextEdit::singleline(&mut self.password).password(true));
        field(ui, "Device name", &mut self.device);
        ui.checkbox(&mut self.register, "Create account with an invite");
        if self.register {
            field(ui, "Invite", &mut self.invite);
        }
        ui.collapsing("Credential storage", |ui| {
            ui.checkbox(
                &mut self.private_file,
                "Store token in private app data on this device",
            );
        });
        ui.add_space(10.);
        if ui.button("Sign in and remember this device").clicked() {
            let url = self.url.clone();
            let login = Login {
                account: self.account.clone(),
                password: std::mem::take(&mut self.password),
                device: self.device.clone(),
                invite: self.register.then(|| std::mem::take(&mut self.invite)),
            };
            let private_file = self.private_file;
            self.job(move |l| {
                l.set_login(&Api::login(&url, login)?, private_file)?;
                Ok(Event::Signed(setup::games(&l)?))
            });
        }
    }
    fn library(&mut self, ui: &mut egui::Ui) {
        ui.heading("Games");
        ui.add(
            egui::TextEdit::singleline(&mut self.filter)
                .hint_text("Search installed and configured games"),
        );
        ui.checkbox(&mut self.server_only, "Server recipes only");
        let mut selection = None;
        egui::ScrollArea::vertical()
            .id_salt("games")
            .max_height((ui.available_height() - 55.).max(80.))
            .show(ui, |ui| {
                for r in &self.games {
                    if self.server_only && !r.shared && !r.mine
                        || !format!("{} {}", r.title, r.id)
                            .to_lowercase()
                            .contains(&self.filter.to_lowercase())
                    {
                        continue;
                    }
                    let label = if r.local {
                        "Configured"
                    } else if r.mine {
                        "My recipe"
                    } else if r.shared {
                        "Shared recipe"
                    } else {
                        "Installed"
                    };
                    if ui
                        .selectable_label(self.selected == r.id, format!("{}\n{}", r.title, label))
                        .clicked()
                    {
                        selection = Some((r.id.clone(), r.title.clone()));
                    }
                }
            });
        if let Some((id, title)) = selection {
            self.select(id, title);
        }
        if ui.button("Add non-Steam game").clicked() {
            self.select(format!("custom-{}", id()), String::new());
        }
    }
    fn game(&mut self, ui: &mut egui::Ui) {
        if self.selected.is_empty() {
            ui.heading("Select a game");
            return;
        }
        let game = self.selected.clone();
        ui.heading(if self.title.is_empty() {
            "New game"
        } else {
            &self.title
        });
        ui.small(&game);
        if self.title.is_empty() || self.selected.starts_with("custom-") {
            field(ui, "Game title", &mut self.title);
        }
        let mut unbind = None;
        let mut import = None;
        if let Some(d) = &self.detail {
            ui.add_space(8.);
            if d.offline {
                ui.label("Offline · showing the setup saved on this device");
            }
            for (label, hash) in &d.shared_options {
                if ui.button(format!("Use shared recipe   {label}")).clicked() {
                    import = Some(hash.clone());
                }
            }
            for candidate in &d.candidates {
                let label = format!(
                    "{}    {}",
                    if candidate.settings {
                        "Settings"
                    } else {
                        "Save"
                    },
                    candidate.path.display()
                );
                if ui
                    .selectable_label(self.path == candidate.path.display().to_string(), label)
                    .clicked()
                {
                    self.path = candidate.path.display().to_string();
                    self.file = candidate.kind == Kind::File;
                    self.settings = candidate.settings;
                    self.source = Some(candidate.source.clone());
                }
            }
            for (k, b) in &d.bindings {
                ui.horizontal_wrapped(|ui| {
                    ui.label(format!("{k}    {}", b.path.display()));
                    if ui.small_button("Stop syncing here…").clicked() {
                        unbind = Some(k.clone());
                    }
                });
            }
            for (name, kind, hint) in &d.slots {
                if ui.button(format!("Configure {name}")).clicked() {
                    self.slot = name.clone();
                    self.file = *kind == Kind::File;
                    self.source = None;
                    self.path = d
                        .bindings
                        .iter()
                        .find(|(k, _)| k == name)
                        .map(|(_, b)| &b.path)
                        .or(hint.as_ref())
                        .map(|p| p.display().to_string())
                        .unwrap_or_default();
                }
            }
        }
        if let Some(hash) = import {
            let g = game.clone();
            self.job(move |l| {
                setup::import(&l, &g, &hash)?;
                Ok(Event::Done("Recipe copied to your account".into()))
            });
        }
        if let Some(slot) = unbind {
            let selected = game.clone();
            self.job(move |l| {
                ensure!(
                    choose(
                        "Stop syncing this selection",
                        &format!("Leave {slot} on disk and keep its server history?"),
                        &["Cancel", "Stop syncing here"]
                    )? == 1,
                    "cancelled"
                );
                setup::unbind(&l, &selected, &slot)?;
                Ok(Event::Done("Selection removed from this device".into()))
            });
        }
        ui.separator();
        ui.label("Files to sync");
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.file, false, "Folder");
            ui.selectable_value(&mut self.file, true, "Single file");
            ui.checkbox(&mut self.settings, "Game settings");
        });
        ui.horizontal(|ui| {
            if ui.text_edit_singleline(&mut self.path).changed() {
                self.source = None;
            }
            if ui.button("Browse…").clicked() {
                self.browse(None);
            }
        });
        let shortcuts = self
            .detail
            .as_ref()
            .map(|d| d.roots.clone())
            .unwrap_or_default();
        ui.horizontal_wrapped(|ui| {
            for (k, p) in shortcuts {
                if !["data", "config", "documents"].contains(&k.as_str())
                    && ui.small_button(k).clicked()
                {
                    self.browse(Some(&p));
                }
            }
        });
        field(ui, "Selection name", &mut self.slot);
        if ui
            .add_enabled(
                !self.path.is_empty() && !self.title.trim().is_empty(),
                egui::Button::new("Save selection"),
            )
            .clicked()
        {
            let sel = Selection {
                game: game.clone(),
                title: self.title.clone(),
                slot: self.slot.clone(),
                path: PathBuf::from(&self.path),
                kind: if self.file { Kind::File } else { Kind::Folder },
                settings: self.settings,
                source: self.source.clone(),
            };
            self.path.clear();
            self.source = None;
            self.job(move |l| {
                setup::bind(&l, sel)?;
                Ok(Event::Done("Selection saved".into()))
            });
        }
        ui.collapsing("Custom root directories", |ui| {
            field(ui, "Name used in the recipe", &mut self.root_name);
            if ui
                .add_enabled(
                    key(&self.root_name),
                    egui::Button::new("Choose directory on this device…"),
                )
                .clicked()
                && let Some(path) = rfd::FileDialog::new().pick_folder()
            {
                let (g, name) = (game.clone(), self.root_name.clone());
                self.job(move |l| {
                    l.bind_root(&g, &name, &path)?;
                    Ok(Event::Done(format!("Directory saved for {name}")))
                });
            }
        });
        let configured = self.detail.as_ref().is_some_and(|d| !d.bindings.is_empty());
        if self.detail.as_ref().is_some_and(|d| d.draft) {
            ui.label("Recipe edits queued on this device");
            ui.horizontal(|ui| {
                if ui.button("Apply edits…").clicked() {
                    let g = game.clone();
                    self.job(move |l| {
                        setup::review(&l, &g, Interface::Window, false)?;
                        Ok(Event::Done("Recipe edits applied".into()))
                    });
                }
                if ui.button("Discard edits…").clicked() {
                    let g = game.clone();
                    self.job(move |l| {
                        ensure!(
                            choose(
                                "Discard recipe edits",
                                "Remove the pending edits on this device?",
                                &["Cancel", "Discard edits"]
                            )? == 1,
                            "cancelled"
                        );
                        setup::review(&l, &g, Interface::Window, true)?;
                        Ok(Event::Done("Recipe edits discarded".into()))
                    });
                }
            });
        }
        if !configured {
            return;
        }
        ui.add_space(8.);
        ui.horizontal_wrapped(|ui| {
            if let Some(launch) = self.detail.as_ref().and_then(|d| d.launcher.clone())
                && ui.button("Play").clicked()
            {
                let selected = game.clone();
                self.job(move |local| {
                    let mut cmd = Command::new(std::env::current_exe()?);
                    cmd.env("BUDDY_HOME", &local.home);
                    cmd.args(["run", "--game", &selected]);
                    if launch.manual_exit {
                        cmd.arg("--manual-exit");
                    }
                    let status = cmd
                        .arg("--")
                        .arg(launch.executable)
                        .args(launch.args)
                        .current_dir(launch.directory)
                        .status()?;
                    Ok(Event::Done(format!("Game closed ({status})")))
                });
            }
            if ui.button("Sync now").clicked() {
                self.sync(None, false);
            }
            if ui.button("Back up now").clicked() {
                self.sync(None, true);
            }
        });
        let fields = std::env::current_exe()
            .map_err(anyhow::Error::from)
            .and_then(|exe| {
                crate::steam_launch::LaunchFields::new(
                    &exe,
                    self.local
                        .explicit_home
                        .then_some(self.local.home.as_path()),
                    &game,
                )
            });
        match fields {
            Ok(f) if f.store_title => {
                if ui.button("Copy launch options").clicked() {
                    ui.ctx().copy_text(f.options);
                    self.note = "Steam launch options copied".into();
                }
            }
            Ok(f) => {
                ui.collapsing("Launch as a non-Steam shortcut", |ui| {
                    ui.label("Save the shortcut’s original Target and Launch Options. Keep its Start In directory.");
                    if ui.button("Copy new Target").clicked(){ui.ctx().copy_text(f.target);}
                    if ui.button("Copy Launch Options prefix").clicked(){ui.ctx().copy_text(f.options);}
                    ui.label("After the prefix, paste the original Target in quotes, followed by its original arguments.");
                });
            }
            Err(e) => {
                ui.label(format!("Unable to format Steam launch options: {e}"));
            }
        }
        egui::CollapsingHeader::new("Backups").show(ui, |ui| {
            let list = self
                .detail
                .as_ref()
                .map(|d| d.backups.clone())
                .unwrap_or_default();
            for b in list {
                ui.horizontal_wrapped(|ui| {
                    ui.label(format!(
                        "#{} · {} · {} {} {}",
                        b.id,
                        b.origin,
                        when(b.time),
                        if b.head { "current" } else { "" },
                        b.reason
                    ));
                    if !b.head && ui.small_button("Restore").clicked() {
                        self.sync(Some(b.id), false);
                    }
                    if b.reason.is_empty() && ui.small_button("Keep").clicked() {
                        self.backup_action(Action::Pin {
                            game: game.clone(),
                            id: b.id,
                        });
                    }
                    if !b.head && ui.small_button("Delete…").clicked() {
                        self.backup_action(Action::Delete {
                            game: game.clone(),
                            id: b.id,
                        });
                    }
                    if b.head && !b.reason.is_empty() && ui.small_button("Remove pin…").clicked()
                    {
                        self.backup_action(Action::UnpinCurrent {
                            game: game.clone(),
                            id: b.id,
                        });
                    }
                });
            }
        });
        ui.collapsing("Advanced", |ui| {
            ui.collapsing("Launch a game directly", |ui| {
                if ui.button("Choose executable…").clicked()
                    && let Some(path) = rfd::FileDialog::new().pick_file()
                {
                    self.launcher.directory = path.parent().unwrap_or(Path::new(".")).to_path_buf();
                    self.launcher.executable = path;
                }
                ui.label(self.launcher.executable.display().to_string());
                if ui.button("Working directory…").clicked()
                    && let Some(path) = rfd::FileDialog::new().pick_folder()
                {
                    self.launcher.directory = path;
                }
                ui.label(self.launcher.directory.display().to_string());
                ui.label("Arguments, one per line");
                ui.text_edit_multiline(&mut self.launcher_args);
                ui.checkbox(
                    &mut self.launcher.manual_exit,
                    "Ask me when the game has closed",
                );
                if ui
                    .add_enabled(
                        !self.launcher.executable.as_os_str().is_empty(),
                        egui::Button::new("Save launcher"),
                    )
                    .clicked()
                {
                    let (g, mut command) = (game.clone(), self.launcher.clone());
                    command.args = self.launcher_args.lines().map(str::to_owned).collect();
                    self.job(move |l| {
                        l.save_launcher(&g, command)?;
                        Ok(Event::Done("Local launcher saved".into()))
                    });
                }
            });
            if let Some(d) = &mut self.detail {
                ui.checkbox(&mut d.shared, "Publish this file recipe to other accounts");
                ui.horizontal(|ui| {
                    ui.label("Previous versions to keep");
                    ui.add(egui::DragValue::new(&mut d.retention).range(0..=1000));
                });
                let (r, s) = (d.retention, d.shared);
                if ui.button("Save policy").clicked() {
                    self.job(move |l| {
                        setup::policy(&l, &game, Some(r), Some(s))?;
                        Ok(Event::Done("Policy saved".into()))
                    });
                }
            }
        });
    }
    fn browse(&mut self, dir: Option<&Path>) {
        let mut dialog = rfd::FileDialog::new();
        if let Some(p) = dir {
            dialog = dialog.set_directory(p);
        }
        if let Some(p) = if self.file {
            dialog.pick_file()
        } else {
            dialog.pick_folder()
        } {
            self.path = p.display().to_string();
            self.source = None;
        }
    }
    fn sync(&mut self, restore: Option<i64>, pin: bool) {
        let game = self.selected.clone();
        self.job(move |l| {
            if let Some(id) = restore {
                ensure!(
                    choose(
                        "Restore backup",
                        &format!("Restore backup #{id} to this device and the server?"),
                        &["Cancel", "Restore"]
                    )? == 1,
                    "cancelled"
                );
            }
            synchronize(
                &l,
                &game,
                RunOptions {
                    restore,
                    pin,
                    ..options()
                },
                Interface::Window,
            )?;
            Ok(Event::Done("Sync complete".into()))
        });
    }
    fn backup_action(&mut self, a: Action) {
        self.job(move |l| {
            if let Action::UnpinCurrent { id,.. } = &a {
                ensure!(choose("Remove permanent pin",&format!("Keep #{id} as the current save and apply normal retention to it once replaced?"),&["Cancel","Remove pin"])? == 1,"cancelled");
            }
            if let Action::Delete { id, .. } = &a {
                ensure!(
                    choose(
                        "Delete backup",
                        &format!("Delete backup #{id}?"),
                        &["Cancel", "Delete backup"]
                    )? == 1,
                    "cancelled"
                );
            }
            Api::new(l.credentials()?)?.rpc::<()>(a)?;
            Ok(Event::Done("Backup updated".into()))
        });
    }
}
fn field(ui: &mut egui::Ui, label: &str, text: &mut String) {
    ui.label(label);
    ui.text_edit_singleline(text);
}
impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
        if let Ok(event) = self.rx.try_recv() {
            self.busy = false;
            match event {
                Ok(Event::Signed(g)) => {
                    self.signed = true;
                    self.games = g;
                }
                Ok(Event::Games(g)) => self.games = g,
                Ok(Event::Detail(d)) => {
                    if d.game == self.selected {
                        self.launcher = d.launcher.clone().unwrap_or_default();
                        self.launcher_args = self.launcher.args.join("\n");
                        if self.path.is_empty() && d.bindings.iter().any(|(k, _)| k == &self.slot) {
                            self.slot = (2..)
                                .map(|n| format!("saves-{n}"))
                                .find(|s| !d.slots.iter().any(|(k, _, _)| k == s))
                                .unwrap();
                        }
                        self.detail = Some(*d);
                    }
                }
                Ok(Event::Done(s)) => {
                    if let Some(r) = self.games.iter_mut().find(|r| r.id == self.selected) {
                        r.local = true;
                        r.mine = true;
                    } else {
                        self.games.push(Row {
                            id: self.selected.clone(),
                            title: self.title.clone(),
                            local: true,
                            mine: true,
                            installed: false,
                            shared: false,
                        });
                    }
                    self.load();
                    self.note = s;
                }
                Err(e) => {
                    self.note = e;
                    self.error = true;
                }
            }
        }
        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("Buddy").size(24.).strong());
                if self.signed {
                    ui.separator();
                    ui.label(format!("{} · {}", self.account, self.device));
                    if ui
                        .add_enabled(!self.busy, egui::Button::new("Refresh"))
                        .clicked()
                    {
                        self.refresh();
                    }
                    if ui
                        .add_enabled(!self.busy, egui::Button::new("Import path catalog…"))
                        .clicked()
                        && let Some(path) = rfd::FileDialog::new()
                            .add_filter("Ludusavi manifest", &["yaml", "yml"])
                            .pick_file()
                    {
                        self.job(move |l| {
                            crate::manifest::import(&l, &path)?;
                            Ok(Event::Games(setup::games(&l)?))
                        });
                    }
                    if ui
                        .add_enabled(!self.busy, egui::Button::new("Switch account"))
                        .clicked()
                    {
                        self.signed = false;
                        self.selected.clear();
                        self.detail = None;
                        self.games.clear();
                    }
                }
            });
        });
        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.horizontal_wrapped(|ui| {
                if self.busy {
                    ui.spinner();
                }
                if self.error {
                    ui.colored_label(ui.visuals().error_fg_color, &self.note);
                } else {
                    ui.label(&self.note);
                }
            });
        });
        if self.signed {
            egui::SidePanel::left("library")
                .default_width(275.)
                .show(ctx, |ui| {
                    ui.add_enabled_ui(!self.busy, |ui| self.library(ui));
                });
        }
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.add_space(10.);
            ui.add_enabled_ui(!self.busy, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    if self.signed {
                        self.game(ui)
                    } else {
                        self.login(ui)
                    }
                });
            });
        });
        if self.busy {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
    }
}
pub fn gui(l: Local) -> Result<()> {
    eframe::run_native(
        "Buddy",
        native([970., 680.]),
        Box::new(move |cc| {
            style(&cc.egui_ctx);
            Ok(Box::new(App::new(l)))
        }),
    )
    .map_err(|e| anyhow!(e.to_string()))
}
pub fn tui(l: &Local) -> Result<()> {
    use inquire::{Confirm, Password, Select, Text};
    loop {
        let item = Select::new(
            "Buddy",
            vec!["Games", "Sign in", "Add non-Steam game", "Quit"],
        )
        .prompt()?;
        if item == "Quit" {
            return Ok(());
        }
        if item == "Sign in" {
            let url = Text::new("Server URL")
                .with_default("http://127.0.0.1:7341")
                .prompt()?;
            let account = Text::new("Account").prompt()?;
            let password = Password::new("Password").without_confirmation().prompt()?;
            let invite = Text::new("Invite (blank for existing account)").prompt()?;
            let device = Text::new("Device")
                .with_default(&default_device())
                .prompt()?;
            let private_file =
                Confirm::new("Store token in a private file instead of the OS credential store?")
                    .with_default(false)
                    .prompt()?;
            l.set_login(
                &Api::login(
                    &url,
                    Login {
                        account,
                        password,
                        device,
                        invite: (!invite.is_empty()).then_some(invite),
                    },
                )?,
                private_file,
            )?;
            continue;
        }
        let (game, title) = if item == "Games" {
            let rows = setup::games(l)?;
            if rows.is_empty() {
                println!("No games found.");
                continue;
            }
            let items = rows
                .iter()
                .map(|r| {
                    format!(
                        "{} ({}){}",
                        r.title,
                        r.id,
                        if r.shared { " · shared" } else { "" }
                    )
                })
                .collect::<Vec<_>>();
            let i = Select::new("Search games", items).raw_prompt()?.index;
            (rows[i].id.clone(), rows[i].title.clone())
        } else {
            (
                format!("custom-{}", id()),
                Text::new("Game title").prompt()?,
            )
        };
        let action = Select::new(
            &title,
            vec![
                "Select files",
                "Sync",
                "Back up now",
                "History",
                "Share and retention",
                "Back",
            ],
        )
        .prompt()?;
        match action {
            "Select files" => {
                let name = Text::new("Slot").with_default("saves").prompt()?;
                let path = PathBuf::from(Text::new("File or directory").prompt()?);
                let kind = if path.is_dir() {
                    Kind::Folder
                } else {
                    Kind::File
                };
                setup::bind(
                    l,
                    Selection {
                        game,
                        title,
                        slot: name,
                        path,
                        kind,
                        settings: false,
                        source: None,
                    },
                )?;
            }
            "Sync" | "Back up now" => {
                synchronize(
                    l,
                    &game,
                    RunOptions {
                        pin: action == "Back up now",
                        ..options()
                    },
                    Interface::Terminal,
                )?;
            }
            "Share and retention" => {
                let s = l.setup(&l.credentials()?, &game)?;
                let share = Confirm::new("Publish this file recipe?")
                    .with_default(s.shared)
                    .prompt()?;
                let count = inquire::CustomType::<u32>::new("Previous copies")
                    .with_default(s.recipe.retention)
                    .prompt()?;
                setup::policy(l, &game, Some(count), Some(share))?;
            }
            "History" => {
                let api = Api::new(l.credentials()?)?;
                let list: Vec<Backup> = api.rpc(Action::Backups { game: game.clone() })?;
                if list.is_empty() {
                    println!("No backups yet.");
                    continue;
                }
                let i = Select::new(
                    "Backup",
                    list.iter()
                        .map(|b| format!("#{} {} {} {}", b.id, b.origin, when(b.time), b.reason))
                        .collect(),
                )
                .raw_prompt()?
                .index;
                let id = list[i].id;
                match Select::new(
                    "Action",
                    vec!["Back", "Keep indefinitely", "Restore", "Delete"],
                )
                .prompt()?
                {
                    "Keep indefinitely" => api.rpc::<()>(Action::Pin { game, id })?,
                    "Restore" => {
                        synchronize(
                            l,
                            &game,
                            RunOptions {
                                restore: Some(id),
                                ..options()
                            },
                            Interface::Terminal,
                        )?;
                    }
                    "Delete"
                        if Confirm::new("Delete backup?")
                            .with_default(false)
                            .prompt()? =>
                    {
                        api.rpc::<()>(Action::Delete { game, id })?
                    }
                    _ => (),
                }
            }
            _ => (),
        }
    }
}
