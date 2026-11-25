use crate::{
    files::Binding,
    local::{Api, Launcher, Local, default_device, roots},
    model::*,
    options,
    session::{Interface, RunOptions, synchronize},
    setup::{self, DeviceState, Row, Selection},
};
use anyhow::{Result, anyhow, ensure};
use eframe::egui::{self, RichText};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::Command,
    sync::mpsc,
    thread,
    time::Duration,
};

mod desktop;
pub(crate) use desktop::{finish_install, list_profiles, steam};
mod dialogs;
pub(crate) use crate::phase::style;
pub(crate) use dialogs::{Cancelled, SaveCopy, choose_conflict, choose_with_details};
pub use dialogs::{Progress, choose, decision_window, progress_window};

pub(crate) fn native(size: [f32; 2]) -> eframe::NativeOptions {
    eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size(size)
            .with_icon(crate::phase::app_icon()),
        ..Default::default()
    }
}

use crate::phase;
mod artwork;
mod code_signin;
pub(crate) use code_signin::{approve_terminal, device_login_terminal};
mod history;
mod offline;
mod onboarding;
mod review;
use onboarding::JoinStep;

struct Detail {
    account_scope: String,
    recipe_revision: i64,
    game: String,
    own: Option<Recipe>,
    retired_slots: std::collections::BTreeSet<String>,
    shared_options: Vec<(String, Recipe)>,
    candidates: Vec<crate::manifest::Suggestion>,
    detected: BTreeMap<PathBuf, Kind>,
    baseline: setup::ReviewBase,
    device: DeviceState,
    has_account_recipe: bool,
    bindings: BTreeMap<String, Binding>,
    named_roots: BTreeMap<String, PathBuf>,
    roots: BTreeMap<String, PathBuf>,
    platform: String,
    cloud_history: bool,
    launcher: Option<Launcher>,
    backups: Vec<Backup>,
    offline: bool,
    draft: bool,
    shared: bool,
    retention: u32,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum GamePage {
    Overview,
    Files,
    Backups,
    Settings,
}
enum Event {
    Signed(Credentials),
    Onboarding(JoinStep),
    Catalog(String),
    Bound,
    ReviewStaged(anyhow::Error),
    Games(Vec<Row>),
    Detail(Box<Detail>),
    History(history::History),
    OfflinePicker(offline::Picker),
    Prepared(String, Result<String>),
    Done(String),
    Notice(String),
    DeviceDialog(desktop::DeviceDialog),
    Restarted,
}
type Reply = Result<Event>;
struct App {
    local: Local,
    rx: mpsc::Receiver<Reply>,
    tx: mpsc::Sender<Reply>,
    busy: bool,
    signed: bool,
    offer_steam: bool,
    device_dialog: Option<desktop::DeviceDialog>,
    join_step: JoinStep,
    url: String,
    account: String,
    password: String,
    device: String,
    invite: String,
    register: bool,
    pairing: Option<crate::host::Pairing>,
    private_file: bool,
    games: Vec<Row>,
    filter: String,
    library_filter: LibraryFilter,
    artwork: artwork::Artwork,
    selected: String,
    title: String,
    page: GamePage,
    review: Option<review::Review>,
    launcher: Launcher,
    launcher_args: String,
    detail: Option<Detail>,
    history: Option<history::History>,
    offline_picker: Option<offline::Picker>,
    note: String,
    error: Option<String>,
}
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum LibraryFilter {
    All,
    Configured,
    Server,
}
impl LibraryFilter {
    fn matches(self, row: &Row) -> bool {
        match self {
            Self::All => true,
            Self::Configured => row.device == DeviceState::Ready,
            Self::Server => row.mine || row.shared,
        }
    }
}

fn phase_heading(ui: &mut egui::Ui, title: &str) {
    ui.heading(
        egui::RichText::new(title)
            .size(34.)
            .strong()
            .color(phase::TEXT),
    );
}

pub(crate) fn error_summary(error: &anyhow::Error) -> String {
    // A caller's context explains which operation failed. Translate bare transport
    // errors only when there is no more specific instruction from that operation.
    let cause: &(dyn std::error::Error + 'static) = error.as_ref();
    if let Some(server) = cause.downcast_ref::<crate::local::ApiError>() {
        return server.1.clone();
    }
    if let Some(http) = cause.downcast_ref::<reqwest::Error>() {
        if crate::local::unreachable(error) {
            return "Couldn’t reach the server. Check the connection and try again.".into();
        }
        if http.is_connect() {
            return "Couldn’t connect. Check the server address and its HTTPS certificate if you use one.".into();
        }
    }
    error.to_string()
}

pub(crate) fn show_problem(ui: &mut egui::Ui, error: &anyhow::Error) {
    let summary = error_summary(error);
    ui.colored_label(ui.visuals().error_fg_color, &summary);
    let details = format!("{error:#}");
    if details != summary {
        ui.collapsing("Details", |ui| ui.monospace(details));
    }
}

fn cover_title(ui: &egui::Ui, rect: egui::Rect, title: &str, y: f32, size: f32) {
    let mut job = egui::text::LayoutJob::simple_singleline(
        title.to_owned(),
        egui::FontId::proportional(size),
        phase::TEXT,
    );
    job.wrap = egui::text::TextWrapping::truncate_at_width((rect.width() - 36.).max(1.));
    let text = ui.painter().layout_job(job);
    ui.painter().galley(
        egui::pos2(rect.left() + 18., y - text.size().y),
        text,
        phase::TEXT,
    );
}

fn game_tile(
    ui: &mut egui::Ui,
    art: &mut artwork::Artwork,
    row: &Row,
    size: egui::Vec2,
) -> egui::Response {
    let label = match row.device {
        DeviceState::Ready => "Configured",
        DeviceState::Paused => "Paused here",
        DeviceState::Pending => "Setup pending",
        DeviceState::NeedsLocations => "Choose locations",
        DeviceState::New if row.mine => "My recipe",
        DeviceState::New if row.shared => "Shared recipe",
        DeviceState::New => "Installed",
    };
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
    response.widget_info(|| {
        egui::WidgetInfo::labeled(
            egui::WidgetType::Button,
            ui.is_enabled(),
            format!("{} · {label}", row.title),
        )
    });
    if ui.is_rect_visible(rect) {
        art.paint(ui, rect, &row.id);
        let stroke = if response.hovered() || response.has_focus() {
            phase::ORANGE
        } else {
            phase::LINE
        };
        ui.painter().rect_stroke(
            rect,
            3.,
            egui::Stroke::new(1.0_f32, stroke),
            egui::StrokeKind::Inside,
        );
        cover_title(ui, rect, &row.title, rect.bottom() - 37., 25.);
        let pos = egui::pos2(rect.left() + 20., rect.bottom() - 21.);
        ui.painter().circle_filled(pos, 2.6, phase::ORANGE);
        ui.painter().text(
            pos + egui::vec2(9., 0.),
            egui::Align2::LEFT_CENTER,
            label,
            egui::FontId::proportional(13.),
            phase::TEXT,
        );
    }
    response.on_hover_text(&row.title)
}

impl App {
    fn new(local: Local, offer_steam: bool) -> Self {
        let (tx, rx) = mpsc::channel();
        let mut app = Self {
            local,
            tx,
            rx,
            busy: false,
            signed: false,
            offer_steam,
            device_dialog: None,
            join_step: JoinStep::Opening,
            url: "http://127.0.0.1:7341".into(),
            account: String::new(),
            password: String::new(),
            device: default_device(),
            invite: String::new(),
            register: false,
            pairing: None,
            private_file: false,
            games: vec![],
            filter: String::new(),
            library_filter: LibraryFilter::All,
            artwork: artwork::Artwork::new(),
            selected: String::new(),
            title: String::new(),
            page: GamePage::Overview,
            review: None,
            launcher: Launcher::default(),
            launcher_args: String::new(),
            detail: None,
            history: None,
            offline_picker: None,
            note: String::new(),
            error: None,
        };
        app.open_profile();
        app
    }
    fn job(&mut self, f: impl FnOnce(Local) -> Result<Event> + Send + 'static) {
        if self.busy {
            return;
        }
        self.busy = true;
        self.error = None;
        self.note.clear();
        let tx = self.tx.clone();
        let local = self.local.clone();
        thread::spawn(move || {
            let _ = tx.send(f(local));
        });
    }
    fn show_error(&mut self, error: anyhow::Error) {
        self.note = error_summary(&error);
        self.error = Some(format!("{error:#}"));
    }
    fn refresh(&mut self) {
        self.job(|l| Ok(Event::Games(setup::games(&l)?)));
    }
    fn load(&mut self) {
        let game = self.selected.clone();
        self.job(move |l| {
            let c = l.credentials()?;
            let s = l.maybe_setup(&c, &game)?;
            let api = Api::new(c.clone())?;
            let (remote, offline) = match api.rpc::<Vec<GameInfo>>(Action::Games) {
                Ok(rows) => (rows.into_iter().find(|r| r.recipe.game == game), false),
                Err(e) if crate::local::unreachable(&e) => (None, true),
                Err(e) => return Err(e),
            };
            let has_account_recipe =
                remote.is_some() || (offline && s.as_ref().is_some_and(|s| s.revision > 0));
            let own_revision = (!offline).then(|| remote.as_ref().map_or(0, |g| g.revision));
            let shared = remote
                .as_ref()
                .map_or_else(|| s.as_ref().is_some_and(|s| s.shared), |r| r.shared);
            let retired_slots = remote
                .as_ref()
                .map(|g| g.retired_slots.clone())
                .or_else(|| s.as_ref().map(|s| s.retired_slots.clone()))
                .unwrap_or_default();
            let own = remote.map(|g| g.recipe).or_else(|| {
                s.as_ref()
                    .filter(|s| !s.recipe.slots.is_empty())
                    .map(|s| s.recipe.clone())
            });
            let retention = own.as_ref().map_or(3, |r| r.retention);
            let shared_options = if own.is_none() && !offline {
                api.rpc::<Vec<GameInfo>>(Action::Catalog)?
                    .into_iter()
                    .filter(|info| info.recipe.game == game)
                    .map(|info| Ok((setup::recipe_hash(&info.recipe)?, info.recipe)))
                    .collect::<Result<Vec<_>>>()?
            } else {
                vec![]
            };
            let named_roots = l.named_roots(&c, &game)?;
            let baseline =
                setup::ReviewBase::new(&l, &c, &game, s.as_ref(), &named_roots, own_revision)?;
            let mut available = roots(&game);
            available.extend(named_roots.clone());
            let (backups, offline) = if has_account_recipe && s.is_some() && !offline {
                match api.rpc(Action::Backups { game: game.clone() }) {
                    Ok(list) => (list, false),
                    Err(e) if crate::local::unreachable(&e) => (vec![], true),
                    Err(e) => return Err(e),
                }
            } else {
                (vec![], offline)
            };
            let device = match s.as_ref() {
                Some(setup)
                    if !setup.bindings.is_empty()
                        && setup
                            .bindings
                            .keys()
                            .all(|name| retired_slots.contains(name))
                        && setup::idle(setup)
                        && setup.draft.is_none() =>
                {
                    DeviceState::Paused
                }
                _ => DeviceState::from_setup(s.as_ref()),
            };
            let mut detail = Detail {
                account_scope: l.scope(&c),
                recipe_revision: own_revision.unwrap_or(0),
                detected: BTreeMap::new(),
                baseline,
                device,
                has_account_recipe,
                platform: setup::runtime(&game),
                bindings: s.as_ref().map(|s| s.bindings.clone()).unwrap_or_default(),
                draft: s.as_ref().is_some_and(|s| s.draft.is_some()),
                shared,
                retention,
                cloud_history: crate::local::has_cloud_history(&game),
                candidates: crate::manifest::candidates(&l, &game)?,
                launcher: l.launcher(&c, &game)?,
                roots: available,
                named_roots,
                game,
                own,
                retired_slots,
                shared_options,
                backups,
                offline,
            };
            detail.detected = review::detected_locations(&detail)?;
            Ok(Event::Detail(Box::new(detail)))
        });
    }
    fn leave_game(&mut self) {
        self.selected.clear();
        self.title.clear();
        self.review = None;
        self.detail = None;
        self.history = None;
        self.note.clear();
        self.error = None;
        self.page = GamePage::Overview;
    }
    fn select(&mut self, game: String, title: String) {
        self.leave_game();
        self.selected = game;
        self.title = title;
        self.load();
    }
    fn finished(&mut self, s: String) {
        self.load();
        self.note = s;
    }
    fn library(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            phase_heading(ui, "Game library");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("+ Non-Steam game").clicked() {
                    self.select(format!("custom-{}", id()), String::new());
                }
            });
        });
        ui.add_space(6.);
        if self.games.iter().any(|r| r.device == DeviceState::Ready)
            && ui.button("Prepare for offline play…").clicked()
        {
            self.job(|local| Ok(Event::OfflinePicker(offline::Picker::load(&local)?)));
        }
        ui.add(
            egui::TextEdit::singleline(&mut self.filter)
                .hint_text("Find a game")
                .desired_width(285.),
        );
        ui.horizontal_wrapped(|ui| {
            for (filter, label) in [
                (LibraryFilter::All, "All games"),
                (LibraryFilter::Configured, "Configured"),
                (LibraryFilter::Server, "Server recipes"),
            ] {
                let count = self.games.iter().filter(|row| filter.matches(row)).count();
                ui.selectable_value(
                    &mut self.library_filter,
                    filter,
                    format!("{label}  {count}"),
                );
            }
        });
        ui.separator();
        let query = self.filter.trim().to_lowercase();
        let matches: Vec<_> = self
            .games
            .iter()
            .filter(|r| {
                self.library_filter.matches(r)
                    && (query.is_empty()
                        || r.title.to_lowercase().contains(&query)
                        || r.id.to_lowercase().contains(&query))
            })
            .collect();
        if matches.is_empty() {
            if !self.busy {
                ui.weak(if self.games.is_empty() {
                    "No games found"
                } else {
                    "No matching games"
                });
            }
            return;
        }
        let columns = if ui.available_width() >= 620. { 2 } else { 1 };
        let height = if columns == 1 { 116. } else { 138. };
        let mut picked = None;
        egui::ScrollArea::vertical()
            .id_salt(("game-cards", &self.filter, self.library_filter))
            .show_rows(ui, height, matches.len().div_ceil(columns), |ui, range| {
                let width = (ui.available_width()
                    - ui.spacing().item_spacing.x * (columns - 1) as f32)
                    / columns as f32;
                for idx in range {
                    let start = idx * columns;
                    ui.horizontal(|ui| {
                        for row in &matches[start..(start + columns).min(matches.len())] {
                            let response =
                                game_tile(ui, &mut self.artwork, row, egui::vec2(width, height));
                            if response.clicked() {
                                picked = Some((row.id.clone(), row.title.clone()));
                            }
                        }
                    });
                }
            });
        if let Some((id, title)) = picked {
            self.select(id, title);
        }
    }
    fn launch_fields(&mut self, ui: &mut egui::Ui, game: &str) {
        let fields = std::env::current_exe()
            .map_err(anyhow::Error::from)
            .and_then(|exe| {
                crate::steam_launch::LaunchFields::new(
                    &exe,
                    self.local
                        .explicit_home
                        .then_some(self.local.home.as_path()),
                    game,
                )
            });
        if ui.button("Manage in Steam…").clicked() {
            self.steam_action(Some(game.to_owned()), None);
        }
        match fields {
            Ok(f) if f.store_title => {
                if ui.button("Copy launch options").clicked() {
                    ui.ctx().copy_text(f.options);
                    self.note = "Steam launch options copied".into();
                    self.error = None;
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
    }
    fn game(&mut self, ui: &mut egui::Ui) {
        if ui.button("Back to library").clicked() {
            self.leave_game();
            return;
        }
        let game = self.selected.clone();
        let (banner, _) =
            ui.allocate_exact_size(egui::vec2(ui.available_width(), 81.), egui::Sense::hover());
        self.artwork.paint(ui, banner, &game);
        ui.painter().rect_stroke(
            banner,
            3.,
            egui::Stroke::new(1.0_f32, phase::LINE),
            egui::StrokeKind::Inside,
        );
        cover_title(
            ui,
            banner,
            if self.title.is_empty() {
                "New game"
            } else {
                &self.title
            },
            banner.bottom() - 15.,
            32.,
        );
        let Some(d) = &self.detail else {
            if !self.busy && ui.button("Retry").clicked() {
                self.load();
            }
            return;
        };
        if d.offline {
            ui.weak(if !d.device.needs_locations() {
                "Offline · saved on this device"
            } else {
                "Connect to the server to set up this game"
            });
        }
        if !d.device.needs_locations() {
            ui.horizontal_wrapped(|ui| {
                for (page, name) in [
                    (GamePage::Overview, "Overview"),
                    (GamePage::Files, "Edit files"),
                    (GamePage::Backups, "Backups"),
                    (GamePage::Settings, "Settings"),
                ] {
                    if ui.selectable_label(self.page == page, name).clicked() && self.page != page {
                        self.page = page;
                        self.review = None;
                    }
                }
            });
            ui.separator();
        }
        if self.page == GamePage::Files && !d.draft {
            if d.device.needs_locations() && game.starts_with("custom-") && d.own.is_none() {
                field(ui, "Game title", &mut self.title);
            }
            ui.heading("Files to sync");
            let review = self.review.get_or_insert_with(|| {
                let shared = if d.shared_options.len() == 1 {
                    Some(d.shared_options[0].0.clone())
                } else {
                    None
                };
                review::Review::new(d, shared)
            });
            #[cfg(target_os = "linux")]
            let new_proton_game = d.device.needs_locations() && game.starts_with("steam-");
            if let Some(action) = review.show(ui, d, &self.title, &self.local.home) {
                if let review::Action::RemoveSlot(slot) = action {
                    let base = d.baseline.clone();
                    self.job(move |local| {
                        let message = format!("Remove {slot} from the recipe for all devices? Local files stay on disk. Unsaved setup edits will be discarded.");
                        ensure!(choose("Remove from recipe", &message, &["Cancel", "Remove"])? == 1, Cancelled);
                        setup::remove_slot(&local, &base, &slot)?;
                        Ok(Event::Done("Location removed from recipe".into()))
                    });
                    return;
                }
                let review::Action::Save(changes) = action else {
                    unreachable!()
                };
                let base = d.baseline.clone();
                let stopping_all = !d.bindings.is_empty()
                    && changes.approved.is_empty()
                    && changes.removed.len() == d.bindings.len();
                let game = game.clone();
                let scope = d.account_scope.clone();
                self.job(move |local| {
                    if !changes.removed.is_empty() {
                        let title = if stopping_all {
                            "Stop syncing this game"
                        } else {
                            "Remove selected locations"
                        };
                        let removed = changes.removed.join(", ");
                        let message = format!(
                            "Stop syncing {removed} on this device? Local files stay on disk."
                        );
                        ensure!(
                            choose(title, &message, &["Cancel", "Save setup"])? == 1,
                            Cancelled
                        );
                    }
                    if let Err(error) = setup::save_review(&local, &base, changes) {
                        let credentials = local.credentials()?;
                        if local.scope(&credentials) == scope
                            && local
                                .maybe_setup(&credentials, &game)?
                                .is_some_and(|s| s.draft.is_some())
                        {
                            return Ok(Event::ReviewStaged(error));
                        }
                        return Err(error);
                    }
                    Ok(Event::Bound)
                });
            }
            #[cfg(target_os = "linux")]
            if new_proton_game {
                ui.collapsing("First Play with Proton", |ui| {
                    ui.label("Set up the Steam launch options, then press Play to initialize the prefix.");
                    self.launch_fields(ui, &game);
                });
            }
            return;
        }
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
                            Cancelled
                        );
                        setup::review(&l, &g, Interface::Window, true)?;
                        Ok(Event::Done("Recipe edits discarded".into()))
                    });
                }
            });
        }
        match self.page {
            GamePage::Files => {}
            GamePage::Backups => self.backups(ui, &game),
            GamePage::Settings => self.game_settings(ui, &game),
            GamePage::Overview => self.overview(ui, &game),
        }
    }
    fn overview(&mut self, ui: &mut egui::Ui, game: &str) {
        let Some(d) = &self.detail else { return };
        phase::panel().show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.label(RichText::new("Synced locations").strong().size(19.));
            ui.separator();
            let live = d
                .bindings
                .iter()
                .filter(|(name, _)| !d.retired_slots.contains(*name));
            if live.clone().next().is_none() {
                ui.weak("No locations enabled on this device");
            }
            for (name, b) in live {
                ui.horizontal_wrapped(|ui| {
                    ui.strong(name);
                    ui.add(
                        egui::Label::new(
                            RichText::new(b.path.display().to_string())
                                .monospace()
                                .size(13.)
                                .color(phase::MUTED),
                        )
                        .wrap(),
                    );
                });
            }
        });
        if let Some(current) = d.backups.iter().find(|b| b.head) {
            ui.label(
                RichText::new(format!(
                    "Current save  ·  {}  ·  {}",
                    current.origin,
                    when(current.time)
                ))
                .monospace()
                .size(13.)
                .color(phase::MUTED),
            );
        }
        let enabled = !d.bindings.is_empty();
        ui.add_space(4.);
        if ui
            .add_enabled(enabled, phase::primary("Sync now"))
            .clicked()
        {
            self.sync(None, false);
        }
        if self.detail.as_ref().is_some_and(|d| d.launcher.is_some()) && ui.button("Play").clicked()
        {
            let selected = game.to_owned();
            self.job(move |local| {
                let status = Command::new(std::env::current_exe()?)
                    .env("BUDDY_HOME", &local.home)
                    .args(["play", &selected])
                    .status()?;
                Ok(Event::Done(format!("Game closed ({status})")))
            });
        }
        self.launch_fields(ui, game);
    }
    fn backups(&mut self, ui: &mut egui::Ui, game: &str) {
        ui.horizontal_wrapped(|ui| {
            ui.heading("Backups");
            if ui
                .add_enabled(
                    self.detail
                        .as_ref()
                        .is_some_and(|d| !d.offline && !d.bindings.is_empty()),
                    phase::primary("Back up now"),
                )
                .clicked()
            {
                self.sync(None, true);
            }
        });
        if self.detail.as_ref().is_some_and(|d| d.offline) {
            ui.weak("Connect to view backups");
            return;
        }
        let list = self
            .detail
            .as_ref()
            .map(|d| d.backups.clone())
            .unwrap_or_default();
        if list.is_empty() {
            ui.weak("No backups yet");
        }
        for (group, title) in [(0, "Current"), (1, "Rolling"), (2, "Permanent")] {
            let mut rows = list
                .iter()
                .filter(|b| {
                    if b.head {
                        group == 0
                    } else if b.reason.is_empty() {
                        group == 1
                    } else {
                        group == 2
                    }
                })
                .peekable();
            if rows.peek().is_none() {
                continue;
            }
            ui.strong(title);
            for b in rows {
                ui.horizontal_wrapped(|ui| {
                    ui.label(format!(
                        "#{} · {} · {} {} {}",
                        b.id,
                        b.origin,
                        when(b.time),
                        if b.head { "current" } else { "" },
                        b.reason
                    ));
                    if ui
                        .selectable_label(
                            self.history.as_ref().is_some_and(|h| h.matches(game, b.id)),
                            "What changed?",
                        )
                        .clicked()
                    {
                        if self.history.as_ref().is_some_and(|h| h.matches(game, b.id)) {
                            self.history = None;
                        } else if let Some(d) = &self.detail {
                            let (account, game, id) =
                                (d.account_scope.clone(), game.to_owned(), b.id);
                            self.job(move |local| {
                                Ok(Event::History(history::History::load(
                                    &local, &account, game, id,
                                )?))
                            });
                        }
                    }
                    if !b.head && ui.small_button("Restore").clicked() {
                        self.sync(Some(b.id), false);
                    }
                    if b.reason.is_empty() && ui.small_button("Keep").clicked() {
                        self.backup_action(Action::Pin {
                            game: game.to_owned(),
                            id: b.id,
                        });
                    }
                    if !b.head && ui.small_button("Delete…").clicked() {
                        self.backup_action(Action::Delete {
                            game: game.to_owned(),
                            id: b.id,
                        });
                    }
                    if b.head && !b.reason.is_empty() && ui.small_button("Remove pin…").clicked()
                    {
                        self.backup_action(Action::UnpinCurrent {
                            game: game.to_owned(),
                            id: b.id,
                        });
                    }
                });
                if let Some(h) = self.history.as_ref().filter(|h| h.matches(game, b.id))
                    && let Some(d) = self.detail.as_ref()
                    && let Some(recipe) = d.own.as_ref()
                    && let Some((path, folder)) = h.show(ui, recipe)
                {
                    let (game, account, rev) =
                        (game.to_owned(), d.account_scope.clone(), d.recipe_revision);
                    let backup = b.id;
                    self.job(move |local| {
                        history::confirm_ignore(&local, &account, &game, backup, &path, folder)?;
                        setup::set_ignore(&local, &game, &path, true, Some((&account, rev)))?;
                        Ok(Event::Done("Recipe updated".into()))
                    });
                }
            }
        }
    }
    fn game_settings(&mut self, ui: &mut egui::Ui, game: &str) {
        ui.heading("Game settings");
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
                let (g, mut command) = (game.to_owned(), self.launcher.clone());
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
                let game = game.to_owned();
                self.job(move |l| {
                    setup::policy(&l, &game, Some(r), Some(s))?;
                    Ok(Event::Done("Policy saved".into()))
                });
            }
        }
    }
    fn sync(&mut self, restore: Option<i64>, pin: bool) {
        let Some(detail) = &self.detail else { return };
        let game = detail.game.clone();
        let expected_scope = Some(detail.account_scope.clone());
        self.job(move |l| {
            synchronize(
                &l,
                &game,
                RunOptions {
                    restore,
                    pin,
                    expected_scope,
                    ..options()
                },
                Interface::Window,
            )?;
            Ok(Event::Done("Sync complete".into()))
        });
    }
    fn backup_action(&mut self, a: Action) {
        let Some(scope) = self.detail.as_ref().map(|d| d.account_scope.clone()) else {
            return;
        };
        self.job(move |l| {
            let credentials = l.credentials()?;
            ensure!(
                l.scope(&credentials) == scope,
                "account changed; reopen the game"
            );
            if let Action::UnpinCurrent { id, .. } = &a {
                let message = format!(
                    "Keep #{id} as the current save and apply normal retention once it is replaced?"
                );
                ensure!(
                    choose("Remove permanent pin", &message, &["Cancel", "Remove pin"])? == 1,
                    Cancelled
                );
            }
            if let Action::Delete { id, .. } = &a {
                ensure!(
                    choose(
                        "Delete backup",
                        &format!("Delete backup #{id}?"),
                        &["Cancel", "Delete backup"]
                    )? == 1,
                    Cancelled
                );
            }
            Api::new(credentials)?.rpc::<()>(a)?;
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
        self.artwork.poll(ctx);
        if let Ok(event) = self.rx.try_recv() {
            self.busy = matches!(&event, Ok(Event::Prepared(..)));
            match event {
                Ok(Event::Signed(credentials)) => {
                    self.remember_profile(&credentials);
                    self.join_step = JoinStep::Welcome;
                    self.signed = true;
                    self.refresh();
                }
                Ok(Event::Onboarding(step)) => {
                    if let JoinStep::SavedSignIn(record, _) = &step {
                        self.remember_profile(record);
                    }
                    if matches!(self.join_step, JoinStep::Storage { .. })
                        && matches!(step, JoinStep::Storage { .. })
                    {
                        self.note = "The credential store is still unavailable.".into();
                    }
                    self.join_step = step;
                }
                Ok(Event::History(view)) => self.history = Some(view),
                Ok(Event::OfflinePicker(picker)) => self.offline_picker = Some(picker),
                Ok(Event::Prepared(game, result)) => self.prepared_result(game, result),
                Ok(Event::Games(g)) => self.games = g,
                Ok(Event::Catalog(msg)) => {
                    if !self.selected.is_empty() {
                        self.load();
                    }
                    self.note = msg;
                }
                Ok(Event::Detail(d)) => {
                    if d.game == self.selected {
                        if let Some(row) = self.games.iter_mut().find(|r| r.id == d.game) {
                            row.device = d.device;
                            row.mine = d.has_account_recipe;
                        } else if d.device != DeviceState::New {
                            self.games.push(Row {
                                id: d.game.clone(),
                                title: self.title.clone(),
                                installed: false,
                                mine: d.has_account_recipe,
                                device: d.device,
                                shared: false,
                            });
                            self.games.sort_by_cached_key(|r| r.title.to_lowercase());
                        }
                        if d.draft && self.note == "Setup saved" {
                            self.note = "Setup queued on this device".into();
                        }
                        self.launcher = d.launcher.clone().unwrap_or_default();
                        self.launcher_args = self.launcher.args.join("\n");
                        if d.device.needs_locations() {
                            self.page = GamePage::Files;
                        }
                        self.review = None;
                        self.detail = Some(*d);
                    }
                }
                Ok(Event::Notice(s)) => self.note = s,
                Ok(Event::DeviceDialog(d)) => self.device_dialog = Some(d),
                Ok(Event::Restarted) => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
                Ok(Event::Done(s)) => {
                    self.finished(s);
                }
                Ok(Event::ReviewStaged(error)) => {
                    self.page = GamePage::Overview;
                    self.review = None;
                    self.finished(format!(
                        "Setup edits are queued on this device. {}",
                        error_summary(&error)
                    ));
                    self.error = Some(format!("{error:#}"));
                }
                Ok(Event::Bound) => {
                    self.page = GamePage::Overview;
                    self.review = None;
                    self.finished("Setup saved".into());
                }
                Err(e) if e.is::<Cancelled>() => {}
                Err(e) => self.show_error(e),
            }
        }
        egui::TopBottomPanel::top("top")
            .frame(
                egui::Frame::new()
                    .fill(phase::PANEL)
                    .inner_margin(egui::Margin::symmetric(17, 10)),
            )
            .show(ctx, |ui| {
                ui.horizontal_wrapped(|ui| {
                    phase::brand(ui);
                    if ui.available_width() > 470. {
                        let width = (ui.available_width() * 0.35).min(365.);
                        let (rect, _) =
                            ui.allocate_exact_size(egui::vec2(width, 24.), egui::Sense::hover());
                        phase::traces(ui.painter(), rect, Some(1.));
                    }
                    {
                        let show_device = self.signed && ui.available_width() > 250.;
                        let layout = egui::Layout::right_to_left(egui::Align::Center);
                        ui.with_layout(layout, |ui| {
                            ui.menu_button("Settings", |ui| {
                                if self.signed {
                                    ui.strong(&self.account);
                                    ui.weak(&self.device);
                                    ui.separator();
                                }
                                self.device_menu(ui);
                                if !self.signed {
                                    return;
                                }
                                ui.separator();
                                if ui.button("Host a server").clicked() {
                                    ui.close();
                                    self.launch_host();
                                }
                                if ui
                                    .add_enabled(!self.busy, egui::Button::new("Refresh"))
                                    .clicked()
                                {
                                    ui.close();
                                    self.artwork = artwork::Artwork::new();
                                    if self.selected.is_empty() {
                                        self.refresh();
                                    } else {
                                        self.load();
                                    }
                                }
                                ui.menu_button("Path suggestions", |ui| {
                                    if ui
                                        .add_enabled(
                                            !self.busy,
                                            egui::Button::new("Download from Ludusavi"),
                                        )
                                        .clicked()
                                    {
                                        ui.close();
                                        self.job(|l| {
                                            Ok(Event::Catalog(crate::manifest::download(&l)?))
                                        });
                                    }
                                    if ui
                                        .add_enabled(
                                            !self.busy,
                                            egui::Button::new("Import local YAML…"),
                                        )
                                        .clicked()
                                    {
                                        ui.close();
                                        if let Some(path) = rfd::FileDialog::new()
                                            .add_filter("Ludusavi manifest", &["yaml", "yml"])
                                            .pick_file()
                                        {
                                            self.job(move |l| {
                                                Ok(Event::Catalog(format!(
                                                    "Indexed {} games.",
                                                    crate::manifest::import(&l, &path)?
                                                )))
                                            });
                                        }
                                    }
                                });
                                if ui
                                    .add_enabled(!self.busy, egui::Button::new("Switch account"))
                                    .clicked()
                                {
                                    ui.close();
                                    self.signed = false;
                                    self.leave_game();
                                    self.games.clear();
                                    self.switch_account();
                                    self.filter.clear();
                                    self.library_filter = LibraryFilter::All;
                                }
                            });
                            if show_device {
                                ui.add(
                                    egui::Label::new(
                                        RichText::new(&self.device).color(phase::MUTED),
                                    )
                                    .truncate(),
                                );
                            }
                        });
                    }
                });
            });
        if self.busy || !self.note.is_empty() {
            egui::TopBottomPanel::bottom("status")
                .frame(
                    egui::Frame::new()
                        .fill(phase::PANEL)
                        .inner_margin(egui::Margin::symmetric(18, 7)),
                )
                .show(ctx, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        if self.busy {
                            ui.spinner();
                        }
                        let color = if self.error.is_some() {
                            ui.visuals().error_fg_color
                        } else {
                            phase::TEXT
                        };
                        ui.colored_label(color, &self.note);
                    });
                    if let Some(details) = &self.error
                        && details != &self.note
                    {
                        ui.collapsing("Details", |ui| {
                            egui::ScrollArea::vertical()
                                .max_height(100.)
                                .show(ui, |ui| ui.monospace(details));
                        });
                    }
                });
        }
        let margin = if ctx.screen_rect().width() < 700. {
            14
        } else {
            26
        };
        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(phase::INK)
                    .inner_margin(egui::Margin::symmetric(margin, 16)),
            )
            .show(ctx, |ui| {
                ui.add_enabled_ui(!self.busy, |ui| {
                    if self.signed && self.selected.is_empty() {
                        self.library(ui);
                    } else {
                        egui::ScrollArea::vertical()
                            .id_salt(("page", &self.selected, self.signed))
                            .show(ui, |ui| {
                                if self.signed {
                                    self.game(ui);
                                } else {
                                    let width = ui.available_width().min(470.);
                                    ui.add_space(8.);
                                    ui.horizontal(|ui| {
                                        ui.add_space(
                                            ((ui.available_width() - width) * 0.5).max(0.),
                                        );
                                        ui.vertical(|ui| {
                                            ui.set_width(width - 10.);
                                            phase::panel().show(ui, |ui| self.login(ui));
                                        });
                                    });
                                }
                            });
                    }
                });
            });
        self.show_device_dialog(ctx);
        self.show_offline_picker(ctx);
        if self.busy {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
    }
}
pub fn gui(l: Local) -> Result<()> {
    open_gui(l, false)
}
fn open_gui(l: Local, offer_steam: bool) -> Result<()> {
    eframe::run_native(
        "Buddy",
        native([970., 680.]),
        Box::new(move |cc| {
            style(&cc.egui_ctx);
            Ok(Box::new(App::new(l, offer_steam)))
        }),
    )
    .map_err(|e| anyhow!(e.to_string()))
}
pub fn tui(l: &Local) -> Result<()> {
    use inquire::{Confirm, Password, Select, Text};
    loop {
        let item = Select::new(
            "Buddy",
            vec![
                "Games",
                "Prepare for offline play",
                "Sign in with a device code",
                "Approve a device",
                "Sign in",
                "Add non-Steam game",
                "Quit",
            ],
        )
        .prompt()?;
        if item == "Quit" {
            return Ok(());
        }
        if item == "Approve a device" {
            let code = Text::new("Code on the device signing in").prompt()?;
            approve_terminal(l, &code)?;
            continue;
        }
        if matches!(item, "Sign in" | "Sign in with a device code") {
            let path = Text::new("Server pairing file (blank to enter a URL)").prompt()?;
            let pairing = (!path.is_empty())
                .then(|| crate::host::Pairing::load(Path::new(&path)))
                .transpose()?;
            let url = if let Some(p) = &pairing {
                println!("{} · {}", p.url, p.fingerprint());
                p.url.clone()
            } else {
                Text::new("Server URL")
                    .with_default("http://127.0.0.1:7341")
                    .prompt()?
            };
            let device = Text::new("Device")
                .with_default(&default_device())
                .prompt()?;
            let private_file = if onboarding::secret_service_absent() {
                if !Confirm::new("Save this device’s token in Buddy’s private app data?")
                    .with_default(false)
                    .prompt()?
                {
                    continue;
                }
                true
            } else {
                Confirm::new("Store token in a private file instead of the OS credential store?")
                    .with_default(false)
                    .prompt()?
            };
            if item == "Sign in with a device code" {
                device_login_terminal(l, &url, pairing.as_ref(), device, private_file)?;
                continue;
            }
            let account = Text::new("Account").prompt()?;
            let password = Password::new("Password").without_confirmation().prompt()?;
            let invite = Text::new("Invite (blank for existing account)").prompt()?;
            l.set_login(
                &Api::login_paired(
                    &url,
                    Login {
                        account,
                        password,
                        device,
                        invite: (!invite.is_empty()).then_some(invite),
                    },
                    pairing.as_ref(),
                )?,
                private_file,
            )?;
            continue;
        }
        if item == "Prepare for offline play" {
            offline::prepare_terminal(l)?;
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
                "Ignore or include a path",
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
                    None,
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
            "Ignore or include a path" => {
                let path = Text::new("Path in recipe (e.g. saves/player/statistics)").prompt()?;
                let action =
                    Select::new("On all devices", vec!["Cancel", "Ignore", "Include again"])
                        .prompt()?;
                if action != "Cancel" {
                    setup::set_ignore(l, &game, &path, action == "Ignore", None)?;
                }
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
                    vec![
                        "Back",
                        "What changed?",
                        "Keep indefinitely",
                        "Restore",
                        "Delete",
                    ],
                )
                .prompt()?
                {
                    "What changed?" => api.backup_changes(&game, id)?.print(),
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
