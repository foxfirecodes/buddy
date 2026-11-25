//! Prepare several configured games without taking over another device's session.
use super::*;
use std::collections::BTreeSet;

pub(super) struct Picker {
    scope: String,
    games: Vec<(String, String)>,
    selected: BTreeSet<String>,
    results: BTreeMap<String, Result<String>>,
}
impl Picker {
    pub(super) fn load(local: &Local) -> Result<Self> {
        let c = local.credentials()?;
        let scope = local.scope(&c);
        let selected = local
            .read(&format!("offline-picks/{scope}"))?
            .unwrap_or_default();
        let mut games = Vec::new();
        let mut results = BTreeMap::new();
        for s in local.setups(&c)? {
            if s.bindings.is_empty() {
                continue;
            }
            let id = s.recipe.game;
            if let Some(prep) = s.offline_prep {
                results.insert(id.clone(), Ok(prep.to_string()));
            }
            games.push((id, s.recipe.title));
        }
        games.sort_by_cached_key(|(_, title)| title.to_lowercase());
        Ok(Self {
            scope,
            games,
            selected,
            results,
        })
    }
}
impl App {
    pub(super) fn show_offline_picker(&mut self, ctx: &egui::Context) {
        let Some(picker) = self.offline_picker.as_mut() else {
            return;
        };
        let mut run = None;
        let mut close = false;
        let mut open = None;
        egui::Modal::new(egui::Id::new("offline-picker")).show(ctx, |ui| {
            ui.set_max_width(520.);
            ui.heading("Prepare for offline play");
            ui.weak("Prepared games open without asking when the server is unreachable.");
            ui.add_space(8.);
            if picker.games.is_empty() {
                ui.label("Set up a game on this device first.");
            }
            egui::ScrollArea::vertical()
                .max_height((ctx.screen_rect().height() * 0.56).max(110.))
                .show(ui, |ui| {
                    for (id, title) in &picker.games {
                        ui.push_id(id, |ui| {
                            let mut checked = picker.selected.contains(id);
                            if ui
                                .add_enabled(!self.busy, egui::Checkbox::new(&mut checked, title))
                                .changed()
                            {
                                if checked {
                                    picker.selected.insert(id.clone());
                                } else {
                                    picker.selected.remove(id);
                                }
                            }
                            if let Some(result) = picker.results.get(id) {
                                match result {
                                    Ok(status) => {
                                        ui.weak(status);
                                    }
                                    Err(e) => {
                                        show_problem(ui, e);
                                        if ui
                                            .add_enabled(!self.busy, egui::Button::new("Open game"))
                                            .clicked()
                                        {
                                            open = Some((id.clone(), title.clone()));
                                        }
                                    }
                                }
                            }
                        });
                    }
                });
            ui.horizontal(|ui| {
                if self.busy {
                    ui.spinner();
                }
                let games: Vec<_> = picker
                    .games
                    .iter()
                    .filter(|(id, _)| picker.selected.contains(id))
                    .map(|(id, _)| id.clone())
                    .collect();
                if ui
                    .add_enabled(
                        !self.busy && !games.is_empty(),
                        phase::primary("Prepare selected"),
                    )
                    .clicked()
                {
                    for id in &games {
                        picker.results.remove(id);
                    }
                    run = Some((picker.scope.clone(), games));
                }
                close = ui
                    .add_enabled(!self.busy, egui::Button::new("Done"))
                    .clicked();
            });
        });
        if let Some((scope, games)) = run {
            let tx = self.tx.clone();
            self.job(move |local| {
                crate::session::prepare_games(&local, &games, &scope, |game, result| {
                    let _ = tx.send(Ok(Event::Prepared(game.into(), result)));
                })?;
                Ok(Event::Notice(String::new()))
            });
        } else if let Some((id, title)) = open {
            self.offline_picker = None;
            self.select(id, title);
        } else if close {
            self.offline_picker = None;
        }
    }

    pub(super) fn prepared_result(&mut self, game: String, result: Result<String>) {
        if let Some(picker) = self.offline_picker.as_mut() {
            picker.results.insert(game, result);
        }
    }
}

pub(super) fn prepare_terminal(local: &Local) -> Result<()> {
    let picker = Picker::load(local)?;
    if picker.games.is_empty() {
        println!("Set up a game on this device first.");
        return Ok(());
    }
    let defaults: Vec<_> = picker
        .games
        .iter()
        .enumerate()
        .filter_map(|(i, (id, _))| picker.selected.contains(id).then_some(i))
        .collect();
    let labels = picker
        .games
        .iter()
        .map(|(id, title)| format!("{title} ({id})"))
        .collect();
    let selected = inquire::MultiSelect::new("Prepare for offline play", labels)
        .with_default(&defaults)
        .raw_prompt()?;
    let games = selected
        .into_iter()
        .map(|item| picker.games[item.index].0.clone())
        .collect::<Vec<_>>();
    crate::session::prepare_games(local, &games, &picker.scope, |game, result| match result {
        Ok(status) => println!("{game} · {status}"),
        Err(e) => println!("{game} · {}", error_summary(&e)),
    })
}
