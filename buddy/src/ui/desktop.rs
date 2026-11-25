use super::{App, Event, choose};
use crate::{
    distribution,
    local::Local,
    phase,
    session::Interface,
    steam_launch::{manage, profiles},
    update,
};
use anyhow::{Context, Result, bail, ensure};
use eframe::egui;
use std::io::IsTerminal;

#[derive(Clone, Copy)]
pub(super) enum ReleaseAction {
    Pair(&'static str),
    Update,
}

pub(super) enum ReleaseDialog {
    Trust(ReleaseAction, String),
    Command(String),
}

pub(crate) fn list_profiles() -> Result<()> {
    for p in profiles::list()? {
        println!("{}\t{}", p.id, p.label);
    }
    Ok(())
}

pub(crate) fn steam(
    local: &Local,
    game: Option<&str>,
    requested: Option<bool>,
    profile: Option<&str>,
    ui: Interface,
    yes: bool,
) -> Result<String> {
    let profiles = profiles::list()?;
    ensure!(
        !profiles.is_empty(),
        "open Steam and sign in to a Steam account on this device first"
    );
    let p = if let Some(id) = profile {
        profiles
            .iter()
            .find(|p| p.id == id)
            .context("unknown Steam profile; use buddy steam profiles")?
    } else if let [p] = profiles.as_slice() {
        p
    } else {
        let names = profiles.iter().map(|p| p.label.as_str());
        let index = match ui {
            Interface::Window => {
                let mut labels = vec!["Cancel"];
                labels.extend(names);
                let index = choose("Choose Steam account", "Apply this change for", &labels)?;
                if index == 0 {
                    return Ok("Cancelled".into());
                }
                index - 1
            }
            Interface::Terminal if !yes => {
                inquire::Select::new("Steam account", names.collect())
                    .raw_prompt()?
                    .index
            }
            _ => {
                bail!("multiple Steam accounts; use buddy steam profiles and pass --steam-profile")
            }
        };
        &profiles[index]
    };
    let install = match requested {
        Some(install) => install,
        None => !manage::is_installed(local, p, game)?,
    };
    let mut plan = manage::plan(local, p, game, install)?;
    if plan.changed && manage::running()? {
        if manage::launched_by_steam() {
            bail!(
                "in Desktop Mode, open {} directly, then repeat this action so Buddy stays open when Steam exits",
                std::env::current_exe()?.display()
            );
        }
        if yes {
            bail!("exit Steam completely, then run this command again");
        }
        loop {
            if ui.choose(
                "Exit Steam",
                "Choose Exit from the Steam menu. Once Steam closes, continue here to review the change.",
                &["Cancel", "Steam has closed"],
            )? == 0 {
                return Ok("Cancelled".into());
            }
            if !manage::running()? {
                break;
            }
        }
        plan = manage::plan(local, p, game, install)?;
    }
    if !plan.changed {
        return Ok(plan.summary);
    }
    if !yes {
        let action = if install { "Install" } else { "Uninstall" };
        let title = format!("{action} in Steam");
        if ui.choose(&title, &plan.summary, &["Cancel", action])? == 0 {
            return Ok("Cancelled".into());
        }
    }
    manage::apply(local, &plan)?;
    Ok(if install {
        "Installed in Steam. You can reopen Steam."
    } else {
        "Removed from Steam. You can reopen Steam."
    }
    .into())
}

pub(crate) fn finish_install(local: Local) -> Result<()> {
    let graphical = cfg!(windows)
        || cfg!(target_os = "macos")
        || ["DISPLAY", "WAYLAND_DISPLAY"]
            .iter()
            .any(|name| std::env::var_os(name).is_some_and(|s| !s.is_empty()));
    if graphical {
        return super::open_gui(local, true);
    }
    if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
        if inquire::Confirm::new("Add Buddy to Steam?")
            .with_default(false)
            .prompt()?
        {
            println!(
                "{}",
                steam(&local, None, Some(true), None, Interface::Terminal, false)?
            );
        }
    } else {
        let exe = std::env::current_exe()?;
        println!(
            "Buddy is installed. To add it to Steam later, run {} steam install.",
            distribution::shell_quote(&exe.to_string_lossy())
        );
    }
    Ok(())
}

impl App {
    pub(super) fn steam_action(&mut self, game: Option<String>, requested: Option<bool>) {
        self.job(move |local| {
            let message = steam(
                &local,
                game.as_deref(),
                requested,
                None,
                Interface::Window,
                false,
            )?;
            Ok(Event::Notice(message))
        });
    }

    fn release_action(&mut self, action: ReleaseAction, key: Option<String>) {
        let account = self.account.clone();
        let server = self.url.clone();
        self.job(move |local| {
            let credentials = local.credentials()?;
            ensure!(
                account == credentials.account
                    && crate::local::canonical_url(&server)? == credentials.url,
                "account changed; reopen Buddy to continue"
            );
            let outcome = match action {
                ReleaseAction::Pair(platform) => {
                    distribution::install_command(&local, &credentials, platform, key.as_deref())
                        .map(|command| Event::ReleaseDialog(ReleaseDialog::Command(command)))
                }
                ReleaseAction::Update => update::run(&local, &credentials, key.as_deref(), || {
                    Ok(choose(
                        "Update Buddy",
                        "Install the verified build from your server and reopen Buddy?",
                        &["Cancel", "Update and reopen"],
                    )? == 1)
                })
                .and_then(|result| match result {
                    update::Outcome::Current => Ok(Event::Notice("Buddy is up to date".into())),
                    update::Outcome::Cancelled => Ok(Event::Notice("Update cancelled".into())),
                    update::Outcome::Updated(exe) => {
                        update::restart_gui(&local, &exe)?;
                        Ok(Event::Restarted)
                    }
                }),
            };
            match outcome {
                Err(error)
                    if error
                        .downcast_ref::<distribution::UntrustedReleaseKey>()
                        .is_some() =>
                {
                    Ok(Event::ReleaseDialog(ReleaseDialog::Trust(
                        action,
                        String::new(),
                    )))
                }
                value => value,
            }
        });
    }

    pub(super) fn device_menu(&mut self, ui: &mut egui::Ui) {
        ui.add_enabled_ui(!self.busy, |ui| {
            if ui.button("Add Buddy to Steam…").clicked() {
                ui.close();
                self.steam_action(None, Some(true));
            }
            if self.signed {
                ui.menu_button("Pair a device", |ui| {
                    for (label, platform) in [
                        ("Linux / Steam Deck", "linux-x86_64"),
                        ("Windows", "windows-x86_64"),
                    ] {
                        if ui.button(label).clicked() {
                            ui.close();
                            self.release_action(ReleaseAction::Pair(platform), None);
                        }
                    }
                });
                if ui.button("Update from server…").clicked() {
                    ui.close();
                    self.release_action(ReleaseAction::Update, None);
                }
            }
        });
    }

    pub(super) fn show_device_dialog(&mut self, ctx: &egui::Context) {
        if self.offer_steam {
            let response = egui::Modal::new("install-offer".into()).show(ctx, |ui| {
                ui.heading("Add Buddy to Steam?");
                ui.horizontal(|ui| {
                    let skip = ui.button("Skip").clicked();
                    let add = ui
                        .add_enabled(!self.busy, phase::primary("Add to Steam…"))
                        .clicked();
                    (skip, add)
                })
                .inner
            });
            if response.inner.1 {
                self.offer_steam = false;
                self.steam_action(None, Some(true));
            } else if response.inner.0 || response.should_close() {
                self.offer_steam = false;
            }
            return;
        }
        let Some(dialog) = &mut self.release_dialog else {
            return;
        };
        let mut retry = None;
        let response = egui::Modal::new("device-release".into()).show(ctx, |ui| {
            ui.set_max_width((ctx.screen_rect().width() - 60.).clamp(160., 520.));
            match dialog {
                ReleaseDialog::Trust(action, key) => {
                    ui.heading("Trust this release key");
                    ui.label(
                        "Run buddy server status on your server and paste the release signing key.",
                    );
                    ui.add(
                        egui::TextEdit::singleline(key)
                            .hint_text("Release key")
                            .desired_width(f32::INFINITY),
                    );
                    ui.horizontal(|ui| {
                        let cancel = ui.button("Cancel").clicked();
                        if ui
                            .add_enabled(!key.trim().is_empty(), phase::primary("Continue"))
                            .clicked()
                        {
                            retry = Some((*action, key.trim().to_owned()));
                        }
                        cancel || retry.is_some()
                    })
                    .inner
                }
                ReleaseDialog::Command(command) => {
                    ui.heading("Pair a device");
                    ui.label("Run this one-time command on your new device.");
                    egui::ScrollArea::vertical()
                        .max_height(130.)
                        .show(ui, |ui| {
                            ui.add(
                                egui::TextEdit::multiline(&mut command.as_str())
                                    .font(egui::TextStyle::Monospace)
                                    .desired_width(f32::INFINITY),
                            );
                        });
                    ui.horizontal(|ui| {
                        let close = ui.button("Close").clicked();
                        if ui.add(phase::primary("Copy command")).clicked() {
                            ctx.copy_text(command.clone());
                            self.note = "Install command copied".into();
                            self.error = None;
                            true
                        } else {
                            close
                        }
                    })
                    .inner
                }
            }
        });
        if response.inner || response.should_close() {
            self.release_dialog = None;
        }
        if let Some((action, key)) = retry {
            self.release_action(action, Some(key));
        }
    }
}
