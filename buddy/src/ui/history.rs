//! Backup changes are fetched when a player opens one revision.
use super::*;
use crate::model::{ChangeKind, FileChange};

pub(super) struct History {
    game: String,
    id: i64,
    comparison: Comparison,
    groups: BTreeMap<String, Vec<FileChange>>,
}

impl History {
    pub(super) fn load(local: &Local, account: &str, game: String, id: i64) -> Result<Self> {
        let c = local.credentials()?;
        ensure!(
            local.scope(&c) == account,
            "Account changed. Reopen the game."
        );
        let changes = Api::new(c)?.backup_changes(&game, id)?;
        let grouped = changes.entries.len() > 20;
        let mut groups = BTreeMap::<String, Vec<FileChange>>::new();
        for entry in changes.entries {
            let parent = if grouped {
                entry.path.rsplit_once('/').map_or("", |(dir, _)| dir)
            } else {
                ""
            };
            groups.entry(parent.to_owned()).or_default().push(entry);
        }
        Ok(Self {
            game,
            id,
            comparison: changes.comparison,
            groups,
        })
    }

    pub(super) fn matches(&self, game: &str, id: i64) -> bool {
        self.game == game && self.id == id
    }

    pub(super) fn show(&self, ui: &mut egui::Ui, recipe: &Recipe) -> Option<(String, bool)> {
        ui.weak(self.comparison.to_string());
        if self.groups.is_empty() {
            ui.label("No files changed");
        }
        let mut ignore = None;
        for (dir, entries) in &self.groups {
            let mut list = |ui: &mut egui::Ui| {
                let height = ui.spacing().interact_size.y;
                egui::ScrollArea::vertical()
                    .id_salt(("changes", self.id, dir))
                    .max_height(220.)
                    .show_rows(ui, height, entries.len(), |ui, rows| {
                        for item in &entries[rows] {
                            phase::file_row(
                                ui,
                                |ui| {
                                    let label = match item.kind {
                                        ChangeKind::Added => "Added",
                                        ChangeKind::Modified => "Modified",
                                        ChangeKind::Removed => "Removed",
                                        ChangeKind::Present => "Present",
                                    };
                                    ui.weak(label);
                                    let name = if dir.is_empty() {
                                        item.path.as_str()
                                    } else {
                                        item.path.rsplit('/').next().unwrap_or(&item.path)
                                    };
                                    ui.add(
                                        egui::Label::new(
                                            RichText::new(if item.directory {
                                                format!("{name}/")
                                            } else {
                                                name.to_owned()
                                            })
                                            .monospace(),
                                        )
                                        .truncate(),
                                    )
                                    .on_hover_text(&item.path);
                                },
                                |ui| {
                                    let (slot, relative) = logical_path(&item.path);
                                    if let Some(slot) = recipe.slots.get(slot) {
                                        if slot.ignores_path(relative) {
                                            ui.weak("Ignored");
                                        } else if !(item.directory && relative.is_empty()) {
                                            phase::overflow_menu(
                                                ui,
                                                &format!("Ignore options for {}", item.path),
                                                |ui| {
                                                    if ui.button("Ignore on all devices…").clicked()
                                                    {
                                                        ignore = Some((
                                                            item.path.clone(),
                                                            item.directory,
                                                        ));
                                                        ui.close();
                                                    }
                                                    for (end, _) in
                                                        item.path.match_indices('/').rev()
                                                    {
                                                        let folder = &item.path[..end];
                                                        if folder.contains('/')
                                                            && ui
                                                                .button(format!(
                                                                    "Ignore {folder}/…"
                                                                ))
                                                                .clicked()
                                                        {
                                                            ignore = Some((folder.into(), true));
                                                            ui.close();
                                                        }
                                                    }
                                                },
                                            );
                                        }
                                    }
                                },
                            )
                        }
                    });
            };
            if dir.is_empty() {
                list(ui);
            } else {
                egui::CollapsingHeader::new(format!("{dir}/  ·  {}", entries.len()))
                    .id_salt((self.id, dir))
                    .show(ui, list);
            }
        }
        ignore
    }
}

pub(super) fn confirm_ignore(
    local: &Local,
    account: &str,
    game: &str,
    backup: i64,
    path: &str,
    folder: bool,
) -> Result<()> {
    let target = if folder {
        let c = local.credentials()?;
        ensure!(
            local.scope(&c) == account,
            "Account changed. Reopen the game."
        );
        let revision: Revision = Api::new(c)?.rpc(Action::ReadRevision {
            game: game.into(),
            id: backup,
        })?;
        let prefix = format!("{path}/");
        let matches = revision
            .tree
            .0
            .range(prefix.clone()..)
            .take_while(|(name, _)| name.starts_with(&prefix))
            .filter_map(|(name, entry)| {
                matches!(entry, Entry::File { .. }).then_some(&name[prefix.len()..])
            });
        let mut sample = Vec::new();
        let mut count = 0;
        for file in matches {
            count += 1;
            if sample.len() < 5 {
                sample.push(file);
            }
        }
        let mut target = format!(
            "{prefix} and everything inside it\n\n{count} matching files in backup #{backup}"
        );
        for name in sample {
            target.push('\n');
            target.push_str(name);
        }
        if count > 5 {
            target.push_str(&format!("\n… and {} more", count - 5));
        }
        target
    } else {
        path.to_owned()
    };
    ensure!(
        choose(
            "Ignore on all devices?",
            &format!("{target}\n\nLocal files and existing backups stay intact."),
            &["Cancel", "Ignore on all devices"]
        )? == 1,
        Cancelled
    );
    Ok(())
}
