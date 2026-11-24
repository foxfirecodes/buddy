//! Device-local selections stay here until the player saves the complete review.
use super::{Detail, phase};
use crate::{
    files::Preview,
    model::{Kind, Recipe, Slot, key},
    setup::{ReviewedSetup, Selection},
};
use eframe::egui::{self, RichText};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::mpsc,
    thread,
};

enum Inventory {
    Waiting,
    Scanning {
        result: mpsc::Receiver<Result<Preview, String>>,
        _alive: mpsc::Sender<()>,
    },
    Ready(Result<Preview, String>),
}
struct Location {
    name: String,
    path: Option<PathBuf>,
    kind: Kind,
    settings: bool,
    source: Option<String>,
    checked: bool,
    inventory: Inventory,
}
impl Location {
    fn at(
        name: String,
        path: Option<PathBuf>,
        kind: Kind,
        settings: bool,
        source: Option<String>,
    ) -> Self {
        Self {
            name,
            path,
            kind,
            settings,
            source,
            checked: false,
            inventory: Inventory::Waiting,
        }
    }
    fn set_path(&mut self, path: PathBuf) {
        self.path = Some(path);
        self.inventory = Inventory::Waiting;
    }
    fn preview(&self) -> Option<&Preview> {
        match &self.inventory {
            Inventory::Ready(Ok(p)) => Some(p),
            _ => None,
        }
    }
    fn unchanged(&self, d: &Detail) -> bool {
        self.path
            .as_ref()
            .is_some_and(|p| d.bindings.get(&self.name).is_some_and(|b| &b.path == p))
    }
}

pub(super) struct Review {
    rows: Vec<Location>,
    inspected: Option<usize>,
    shared: Option<String>,
    browse_from: Option<PathBuf>,
    root_name: String,
    root_changes: BTreeMap<String, PathBuf>,
}
fn new_name(path: &Path, rows: &[Location]) -> String {
    let base: String = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("saves")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .take(70)
        .collect();
    let base = base.trim_matches('-');
    let base = if base.is_empty() { "saves" } else { base };
    if rows.iter().all(|r| r.name != base) {
        return base.into();
    }
    (2..)
        .map(|n| format!("{base}-{n}"))
        .find(|s| rows.iter().all(|r| r.name != *s))
        .unwrap()
}
fn active_recipe<'a>(d: &'a Detail, shared: Option<&str>) -> Option<&'a Recipe> {
    d.own.as_ref().or_else(|| {
        d.shared_options
            .iter()
            .find(|(hash, _)| Some(hash.as_str()) == shared)
            .map(|(_, r)| r)
    })
}
fn slot_location<'a>(
    slot: &'a Slot,
    d: &Detail,
    changes: &BTreeMap<String, PathBuf>,
) -> Option<&'a crate::model::Location> {
    slot.locations.get(&d.platform).or_else(|| {
        slot.locations
            .values()
            .find(|l| d.named_roots.contains_key(&l.root) || changes.contains_key(&l.root))
    })
}
fn resolve(slot: &Slot, d: &Detail, changes: &BTreeMap<String, PathBuf>) -> Option<PathBuf> {
    let location = slot_location(slot, d, changes)?;
    changes
        .get(&location.root)
        .or_else(|| d.roots.get(&location.root))
        .map(|base| base.join(&location.relative))
}
/// Cached while loading a game; checking and rendering a recipe never walks the filesystem on the GUI thread.
pub(super) fn detected_locations(d: &Detail) -> anyhow::Result<BTreeMap<PathBuf, Kind>> {
    let mut detected = BTreeMap::new();
    let roots: BTreeSet<_> = d
        .roots
        .values()
        .filter_map(|r| r.canonicalize().ok())
        .collect();
    let mut consider = |path: PathBuf, kind: &Kind, base: Option<&PathBuf>| {
        let Some(actual) = path.canonicalize().ok() else {
            return;
        };
        if roots.iter().any(|root| root.starts_with(&actual))
            || base.is_some_and(|root| {
                root.canonicalize()
                    .map_or(true, |base| !actual.starts_with(base))
            })
        {
            return;
        }
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            return;
        };
        let matches = match kind {
            Kind::File => meta.is_file(),
            Kind::Folder => meta.is_dir(),
        };
        if matches {
            detected.insert(path, kind.clone());
        }
    };
    for recipe in d.own.iter().chain(d.shared_options.iter().map(|(_, r)| r)) {
        recipe.validate()?;
        for slot in recipe.slots.values() {
            if let Some(path) = resolve(slot, d, &BTreeMap::new()) {
                let base =
                    slot_location(slot, d, &BTreeMap::new()).and_then(|l| d.roots.get(&l.root));
                consider(path, &slot.kind, base);
            }
        }
    }
    for c in &d.candidates {
        consider(c.path.clone(), &c.kind, None);
    }
    Ok(detected)
}
impl Review {
    pub(super) fn new(d: &Detail, shared: Option<String>) -> Self {
        let mut rows = Vec::new();
        if let Some(recipe) = active_recipe(d, shared.as_deref()) {
            for (name, slot) in &recipe.slots {
                let binding = d.bindings.get(name);
                let path = binding
                    .map(|b| b.path.clone())
                    .or_else(|| resolve(slot, d, &BTreeMap::new()));
                let mut row = Location::at(
                    name.clone(),
                    path,
                    slot.kind.clone(),
                    slot.settings,
                    slot.source.clone(),
                );
                row.checked = binding.is_some()
                    || (d.device.needs_locations()
                        && !row.settings
                        && row
                            .path
                            .as_ref()
                            .is_some_and(|p| d.detected.get(p) == Some(&row.kind)));
                rows.push(row);
            }
        }
        // Preserve a device binding even if a remote edit no longer contains its slot.
        for (name, b) in &d.bindings {
            if rows.iter().all(|r| r.name != *name)
                && let Some(slot) = d.local_slots.get(name)
            {
                let mut row = Location::at(
                    name.clone(),
                    Some(b.path.clone()),
                    slot.kind.clone(),
                    slot.settings,
                    slot.source.clone(),
                );
                row.checked = true;
                rows.push(row);
            }
        }
        for c in &d.candidates {
            if rows.iter().any(|r| r.path.as_ref() == Some(&c.path)) {
                continue;
            }
            let name = c
                .slot
                .map(str::to_owned)
                .unwrap_or_else(|| new_name(&c.path, &rows));
            if let Some(row) = rows
                .iter_mut()
                .find(|r| r.name == name && r.path.is_none() && r.kind == c.kind)
            {
                row.path = Some(c.path.clone());
                row.checked = d.device.needs_locations()
                    && !row.settings
                    && !c.settings
                    && d.detected.get(&c.path) == Some(&c.kind);
                continue;
            }
            if let Some(slot) = active_recipe(d, shared.as_deref()).and_then(|r| r.slots.get(&name))
                && (slot.kind != c.kind || slot.settings != c.settings)
            {
                continue;
            }
            let mut row = Location::at(
                name,
                Some(c.path.clone()),
                c.kind.clone(),
                c.settings,
                Some(c.source.clone()),
            );
            row.checked = d.device.needs_locations()
                && c.slot.is_some()
                && !c.settings
                && d.detected.get(&c.path) == Some(&c.kind);
            rows.push(row);
        }
        // Alternatives for a logical slot or overlapping trees require an explicit pick.
        let ambiguous: Vec<_> = rows
            .iter()
            .enumerate()
            .map(|(i, a)| {
                rows.iter().enumerate().any(|(j, b)| {
                    i != j
                        && (a.name == b.name
                            || (a
                                .path
                                .as_ref()
                                .zip(b.path.as_ref())
                                .is_some_and(|(x, y)| x.starts_with(y) || y.starts_with(x))))
                })
            })
            .collect();
        for (row, ambiguous) in rows.iter_mut().zip(ambiguous) {
            if ambiguous && !row.unchanged(d) {
                row.checked = false;
            }
        }
        let inspected = rows.iter().position(|r| r.checked);
        Self {
            rows,
            inspected,
            shared,
            browse_from: None,
            root_name: String::new(),
            root_changes: BTreeMap::new(),
        }
    }
    fn inventory(&mut self, ui: &egui::Ui, home: &Path) {
        for row in &mut self.rows {
            if let Inventory::Scanning { result, .. } = &row.inventory {
                match result.try_recv() {
                    Ok(value) => row.inventory = Inventory::Ready(value),
                    Err(mpsc::TryRecvError::Empty) => return,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        row.inventory = Inventory::Ready(Err("File preview stopped".into()))
                    }
                }
            }
        }
        // Inventory one candidate at a time, prioritizing the path currently inspected.
        let waiting = |r: &Location| {
            matches!(r.inventory, Inventory::Waiting) && r.path.is_some() && key(&r.name)
        };
        let next = self
            .inspected
            .filter(|i| waiting(&self.rows[*i]))
            .or_else(|| self.rows.iter().position(|r| r.checked && waiting(r)));
        if let Some(i) = next {
            let row = &mut self.rows[i];
            let (home, path, kind, name) = (
                home.to_owned(),
                row.path.clone().unwrap(),
                row.kind.clone(),
                row.name.clone(),
            );
            let (tx, rx) = mpsc::channel();
            let (alive, cancel) = mpsc::channel();
            let ctx = ui.ctx().clone();
            thread::spawn(move || {
                let v = crate::files::preview(&home, &path, kind, &name, || {
                    cancel.try_recv() == Err(mpsc::TryRecvError::Disconnected)
                });
                let _ = tx.send(v.map_err(|e| format!("{e:#}")));
                ctx.request_repaint();
            });
            row.inventory = Inventory::Scanning {
                result: rx,
                _alive: alive,
            };
        }
    }
    fn pick(&mut self, index: usize, checked: bool) {
        if checked {
            for i in 0..self.rows.len() {
                if i != index && self.rows[i].name == self.rows[index].name {
                    self.rows[i].checked = false;
                }
            }
        }
        self.rows[index].checked = checked;
        self.inspected = Some(index);
    }
    fn browse(&mut self, kind: Kind, index: Option<usize>) {
        let mut dialog = rfd::FileDialog::new();
        if let Some(root) = self.browse_from.as_ref() {
            dialog = dialog.set_directory(root);
        }
        let path = match kind {
            Kind::Folder => dialog.pick_folder(),
            Kind::File => dialog.pick_file(),
        };
        let Some(path) = path else { return };
        let index = if let Some(i) = index {
            self.rows[i].set_path(path);
            i
        } else if let Some(i) = self
            .rows
            .iter()
            .position(|r| r.path.as_ref() == Some(&path))
        {
            i
        } else {
            let name = new_name(&path, &self.rows);
            self.rows
                .push(Location::at(name, Some(path), kind, false, None));
            self.rows.len() - 1
        };
        self.pick(index, true);
    }
    fn rows(&mut self, ui: &mut egui::Ui) {
        let mut pick = None;
        if self.rows.is_empty() {
            ui.weak("No locations detected");
        }
        for (i, r) in self.rows.iter().enumerate() {
            ui.push_id(i, |ui| {
                ui.horizontal(|ui| {
                    let mut checked = r.checked;
                    if ui
                        .add_enabled(r.path.is_some(), egui::Checkbox::without_text(&mut checked))
                        .on_hover_text(format!("Sync {}", r.name))
                        .changed()
                    {
                        pick = Some((i, checked));
                    }
                    ui.vertical(|ui| {
                        let row_label = if r.settings {
                            format!("{} · Settings", r.name)
                        } else {
                            r.name.clone()
                        };
                        if ui
                            .selectable_label(self.inspected == Some(i), row_label)
                            .clicked()
                        {
                            self.inspected = Some(i);
                        }
                        if let Some(path) = r.path.as_ref() {
                            ui.add(
                                egui::Label::new(
                                    RichText::new(path.display().to_string())
                                        .monospace()
                                        .size(12.)
                                        .color(phase::MUTED),
                                )
                                .truncate(),
                            )
                            .on_hover_text(path.display().to_string());
                        } else {
                            ui.weak("Choose a location");
                        }
                    });
                });
                ui.separator();
            });
        }
        if let Some((i, checked)) = pick {
            self.pick(i, checked);
        }
    }
    fn map_root(&mut self, d: &Detail, name: &str) {
        let Some(path) = rfd::FileDialog::new()
            .set_title(format!("Choose the {name} directory"))
            .pick_folder()
        else {
            return;
        };
        self.root_changes.insert(name.to_owned(), path);
        if let Some(recipe) = active_recipe(d, self.shared.as_deref()) {
            for row in &mut self.rows {
                if !row.unchanged(d)
                    && let Some(slot) = recipe.slots.get(&row.name)
                    && slot_location(slot, d, &self.root_changes).is_some_and(|l| l.root == name)
                    && let Some(path) = resolve(slot, d, &self.root_changes)
                {
                    row.set_path(path);
                }
            }
        }
    }
    fn included(&mut self, ui: &mut egui::Ui, d: &Detail) {
        let Some(i) = self.inspected else {
            ui.weak("Select a location to preview");
            return;
        };
        let row = &mut self.rows[i];
        ui.strong(&row.name);
        if let Some(path) = &row.path {
            ui.add(
                egui::Label::new(
                    RichText::new(path.display().to_string())
                        .monospace()
                        .size(13.),
                )
                .wrap(),
            );
        }
        match &row.inventory {
            Inventory::Ready(Ok(p)) => {
                ui.label(format!(
                    "{} · {}",
                    file_count(p.files.len()),
                    bytes(p.bytes)
                ));
                if !p.files.is_empty() {
                    let h = ui.text_style_height(&egui::TextStyle::Monospace);
                    egui::ScrollArea::vertical()
                        .id_salt(("review-files", i))
                        .max_height(140.)
                        .show_rows(ui, h, p.files.len(), |ui, r| {
                            for f in &p.files[r] {
                                ui.add(
                                    egui::Label::new(RichText::new(f).monospace().size(13.))
                                        .truncate(),
                                )
                                .on_hover_text(f);
                            }
                        });
                }
            }
            Inventory::Ready(Err(e)) => {
                ui.colored_label(ui.visuals().error_fg_color, e);
            }
            _ if row.path.is_some() => {
                ui.weak("Counting files…");
            }
            _ => {}
        }
        if let Some(source) = &row.source {
            ui.label(RichText::new(source).size(12.).color(phase::MUTED))
                .on_hover_text(source);
        }
        let slot = active_recipe(d, self.shared.as_deref()).and_then(|r| r.slots.get(&row.name));
        let fixed = slot.is_some();
        let mut missing_roots = BTreeMap::new();
        if row.path.is_none()
            && let Some(slot) = slot
        {
            for (platform, loc) in &slot.locations {
                if crate::local::validate_root_name(&loc.root).is_ok()
                    && !d.named_roots.contains_key(&loc.root)
                    && !self.root_changes.contains_key(&loc.root)
                    && (!slot.locations.contains_key(&d.platform) || platform == &d.platform)
                {
                    missing_roots.insert(&loc.root, &loc.relative);
                }
            }
        }
        let map_root = missing_roots.into_iter().find_map(|(name, relative)| {
            ui.button(format!("Map {name}…"))
                .on_hover_text(format!("{name}/{relative}"))
                .clicked()
                .then_some(name.clone())
        });
        let mut redo = false;
        ui.collapsing("Location options", |ui| {
            if !fixed {
                ui.label("Selection name");
                if ui.text_edit_singleline(&mut row.name).changed() {
                    row.inventory = Inventory::Waiting;
                }
                ui.checkbox(&mut row.settings, "Game settings");
            }
            if row.path.is_some() {
                redo = ui.button("Change location…").clicked();
            }
        });
        if (row.path.is_none() && ui.button("Choose location…").clicked()) || redo {
            let kind = row.kind.clone();
            self.browse(kind, Some(i));
        }
        if let Some(name) = map_root {
            self.map_root(d, &name);
        }
    }
    fn selection(&self, d: &Detail, title: &str) -> Result<ReviewedSetup, String> {
        if d.offline && d.device == crate::setup::DeviceState::New {
            return Err("Connect to the server to enroll this game".into());
        }
        if title.trim().is_empty() {
            return Err("Enter a game title".into());
        }
        let mut approved = vec![];
        let mut names = BTreeSet::new();
        let mut paths: Vec<&Path> = vec![];
        for r in self.rows.iter().filter(|r| r.checked) {
            if !key(&r.name) || !names.insert(r.name.clone()) {
                return Err("Use a unique selection name for each location".into());
            }
            let path = r.path.as_deref().ok_or("Choose a location")?;
            if paths
                .iter()
                .any(|p| p.starts_with(path) || path.starts_with(p))
            {
                return Err("Selected locations overlap".into());
            }
            paths.push(path);
            if !r.unchanged(d) {
                let binding = r
                    .preview()
                    .ok_or("Review the selected locations")?
                    .binding
                    .clone();
                approved.push((
                    Selection {
                        game: d.game.clone(),
                        title: title.to_owned(),
                        slot: r.name.clone(),
                        path: path.to_owned(),
                        kind: r.kind.clone(),
                        settings: r.settings,
                        source: r.source.clone(),
                    },
                    binding,
                ));
            }
        }
        let removed: Vec<_> = d
            .bindings
            .keys()
            .filter(|k| !names.contains(*k))
            .cloned()
            .collect();
        if d.device.needs_locations() && approved.is_empty() {
            return Err("Select a save location".into());
        }
        if approved.is_empty() && removed.is_empty() && self.root_changes.is_empty() {
            return Err("No changes to save".into());
        }
        Ok(ReviewedSetup {
            title: title.to_owned(),
            approved,
            removed,
            shared_recipe: self.shared.clone(),
            root_changes: self.root_changes.clone(),
        })
    }
    pub(super) fn show(
        &mut self,
        ui: &mut egui::Ui,
        d: &Detail,
        title: &str,
        home: &Path,
    ) -> Option<ReviewedSetup> {
        self.inventory(ui, home);
        if d.own.is_none() && !d.shared_options.is_empty() {
            let mut shared = self.shared.clone();
            egui::ComboBox::from_label("File recipe")
                .selected_text(
                    shared
                        .as_ref()
                        .and_then(|h| d.shared_options.iter().find(|(k, _)| k == h))
                        .map(|(_, r)| r.title.as_str())
                        .unwrap_or("Detected locations"),
                )
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut shared, None, "Detected locations");
                    for (hash, recipe) in &d.shared_options {
                        ui.selectable_value(
                            &mut shared,
                            Some(hash.clone()),
                            format!(
                                "{} · {} · {}",
                                recipe.title,
                                recipe.slots.keys().cloned().collect::<Vec<_>>().join(", "),
                                &hash[..8]
                            ),
                        );
                    }
                });
            if shared != self.shared {
                *self = Self::new(d, shared);
            }
        }
        ui.horizontal_wrapped(|ui| {
            ui.menu_button("Add files…", |ui| {
                ui.label("Start in");
                let unchanged_roots = d.roots.iter().filter(|(name,_)| !self.root_changes.contains_key(*name));
                for (name, path) in unchanged_roots.chain(&self.root_changes) {
                    let label = match name.as_str() {
                        "install" => "Steam folder",
                        "pfx" => "Proton prefix",
                        "home" => "Home",
                        "documents" => "Documents",
                        "config" | "data" => continue,
                        other => other,
                    };
                    ui.selectable_value(&mut self.browse_from, Some(path.clone()), label)
                        .on_hover_text(path.display().to_string());
                }
                ui.separator();
                for (kind, label) in [(Kind::Folder,"Choose folder…"), (Kind::File,"Choose file…")] {
                    if ui.button(label).clicked(){
                        ui.close();
                        self.browse(kind, None);
                    }
                }
            });
            if d.cloud_history {
                ui.label(RichText::new("Steam Cloud records found").color(phase::ORANGE))
                    .on_hover_text("Disable overlapping Steam Cloud saves before syncing the same files with Buddy.");
            }
        });
        if ui.available_width() >= 660. {
            ui.columns(2, |cols| {
                phase::panel().show(&mut cols[0], |ui| {
                    ui.set_min_width(ui.available_width());
                    ui.label(RichText::new("Locations").size(19.).strong());
                    ui.separator();
                    self.rows(ui);
                });
                phase::panel().show(&mut cols[1], |ui| {
                    ui.set_min_width(ui.available_width());
                    ui.label(RichText::new("Included files").size(19.).strong());
                    ui.separator();
                    self.included(ui, d);
                });
            });
        } else {
            phase::panel().show(ui, |ui| {
                ui.set_min_width(ui.available_width());
                ui.label(RichText::new("Locations").size(19.).strong());
                ui.separator();
                self.rows(ui);
            });
            phase::panel().show(ui, |ui| {
                ui.set_min_width(ui.available_width());
                ui.label(RichText::new("Included files").size(19.).strong());
                ui.separator();
                self.included(ui, d);
            });
        }
        ui.collapsing("Custom root directories", |ui| {
            for (k, v) in d
                .named_roots
                .iter()
                .filter(|(k, _)| !self.root_changes.contains_key(*k))
                .chain(self.root_changes.iter())
            {
                ui.label(format!("{k}  ·  {}", v.display()));
            }
            ui.label("Root name");
            ui.text_edit_singleline(&mut self.root_name);
            if ui
                .add_enabled(
                    crate::local::validate_root_name(&self.root_name).is_ok(),
                    egui::Button::new("Choose directory…"),
                )
                .clicked()
            {
                let name = self.root_name.clone();
                self.map_root(d, &name);
                if self.root_changes.contains_key(&name) {
                    self.root_name.clear();
                }
            }
        });
        let total: Vec<_> = self.rows.iter().filter(|r| r.checked).collect();
        let done = total.iter().all(|r| r.preview().is_some());
        let count = total
            .iter()
            .filter_map(|r| r.preview())
            .map(|p| p.files.len())
            .sum();
        let size = total
            .iter()
            .filter_map(|r| r.preview())
            .map(|p| p.bytes)
            .sum();
        let selected = self.selection(d, title);
        let tip = selected.as_ref().err().cloned().unwrap_or_default();
        let save = ui
            .horizontal_wrapped(|ui| {
                let clicked = ui
                    .add_enabled(selected.is_ok(), phase::primary("Save setup"))
                    .on_disabled_hover_text(tip)
                    .clicked();
                if done {
                    ui.label(
                        RichText::new(format!("{} · {}", file_count(count), bytes(size)))
                            .monospace()
                            .color(phase::MUTED),
                    );
                }
                clicked
            })
            .inner;
        if save {
            return selected.ok();
        }
        None
    }
}
fn file_count(n: usize) -> String {
    format!("{n} {}", if n == 1 { "file" } else { "files" })
}
fn bytes(n: u64) -> String {
    if n < 1024 {
        format!("{n} B")
    } else if n < 1 << 20 {
        format!("{:.1} KiB", n as f64 / 1024.)
    } else {
        format!("{:.1} MiB", n as f64 / (1 << 20) as f64)
    }
}
