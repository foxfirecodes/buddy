use super::{App, Event, JoinStep, desktop::DeviceDialog, onboarding::save_sign_in, phase_heading};
use crate::{
    device_code::{self, Pending},
    local::{ApiError, default_device},
    phase,
};
use anyhow::{Result, ensure};
use eframe::egui;
use std::{
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

enum CodeState {
    Waiting { next: Instant, retrying: bool },
    Polling(mpsc::Receiver<Result<Option<String>>>),
    Approved(String),
    Expired,
}

pub(super) struct CodeScreen {
    pending: Pending,
    private_only: bool,
    state: CodeState,
}

impl App {
    pub(super) fn start_device_code(&mut self, private_only: bool) {
        let url = self.url.clone();
        let pairing = self.pairing.clone();
        let device = if self.device.trim().is_empty() {
            default_device()
        } else {
            self.device.clone()
        };
        self.password.clear();
        self.job(move |_| {
            let pending = Pending::start(&url, pairing.as_ref(), device)?;
            Ok(Event::Onboarding(JoinStep::DeviceCode(Box::new(
                CodeScreen {
                    pending,
                    private_only,
                    state: CodeState::Waiting {
                        next: Instant::now() + Duration::from_secs(3),
                        retrying: false,
                    },
                },
            ))))
        });
    }

    pub(super) fn device_code_screen(&mut self, ui: &mut egui::Ui) {
        let JoinStep::DeviceCode(screen) = &mut self.join_step else {
            return;
        };
        let private_only = screen.private_only;
        let now = Instant::now();
        if now >= screen.pending.deadline {
            screen.state = CodeState::Expired;
        }
        if let CodeState::Polling(rx) = &screen.state
            && let Ok(result) = rx.try_recv()
        {
            screen.state = match result {
                Ok(Some(account)) => CodeState::Approved(account),
                Ok(None) => CodeState::Waiting {
                    next: now + Duration::from_secs(3),
                    retrying: false,
                },
                Err(e) if e.downcast_ref::<ApiError>().is_some_and(|e| e.0 == 410) => {
                    CodeState::Expired
                }
                Err(_) => CodeState::Waiting {
                    next: now + Duration::from_secs(6),
                    retrying: true,
                },
            };
        }
        phase_heading(ui, "Device sign-in");
        ui.weak(&self.url);
        match &screen.state {
            CodeState::Expired => {
                ui.label("Code expired.");
                if ui.add(phase::primary("Get a new code")).clicked() {
                    self.start_device_code(private_only);
                    return;
                }
            }
            CodeState::Approved(account) => {
                if private_only {
                    ui.weak("Your sign-in will be saved in Buddy’s private app data.");
                } else {
                    ui.collapsing("Credential storage", |ui| {
                        ui.checkbox(
                            &mut self.private_file,
                            "Store token in private app data on this device",
                        );
                    });
                }
                if ui
                    .add_sized(
                        [ui.available_width(), 46.],
                        phase::primary(&format!("Sign in as {account}")),
                    )
                    .clicked()
                {
                    let pending = screen.pending.clone();
                    let account = account.clone();
                    let private = private_only || self.private_file;
                    self.job(move |l| save_sign_in(&l, pending.finish(&account)?, private));
                    return;
                }
            }
            CodeState::Polling(_) | CodeState::Waiting { .. } => {
                ui.label(
                    egui::RichText::new(screen.pending.display_code())
                        .monospace()
                        .size(42.)
                        .color(phase::ORANGE),
                );
                ui.label("Choose Approve a device in Settings on your signed-in device.");
                let remaining = screen
                    .pending
                    .deadline
                    .saturating_duration_since(now)
                    .as_secs();
                ui.weak(format!(
                    "Expires in {}:{:02}",
                    remaining / 60,
                    remaining % 60
                ));
                if let CodeState::Waiting { next, retrying } = screen.state {
                    if retrying {
                        ui.label("Reconnecting to the server…");
                    }
                    if now >= next {
                        let (tx, rx) = mpsc::channel();
                        let pending = screen.pending.clone();
                        let ctx = ui.ctx().clone();
                        thread::spawn(move || {
                            let _ = tx.send(pending.poll());
                            ctx.request_repaint();
                        });
                        screen.state = CodeState::Polling(rx);
                    }
                }
            }
        }
        ui.ctx().request_repaint_after(Duration::from_secs(1));
        if ui.add(phase::quiet("Cancel")).clicked() {
            let pending = screen.pending.clone();
            self.join_step = JoinStep::Account { private_only };
            thread::spawn(move || {
                let _ = pending.cancel();
            });
        }
    }

    pub(super) fn show_approval_dialog(&mut self, ctx: &egui::Context) {
        let mut lookup = None;
        let mut approval = None;
        let response = phase::modal("approve-device").show(ctx, |ui| {
            ui.set_max_width((ctx.screen_rect().width() - 50.).clamp(200., 440.));
            ui.heading("Approve a device");
            if self.error.is_some() {
                ui.colored_label(ui.visuals().error_fg_color, &self.note);
            }
            ui.add_enabled_ui(!self.busy, |ui| match self.device_dialog.as_mut() {
                Some(DeviceDialog::EnterCode(input)) => {
                    let field = ui.add(
                        egui::TextEdit::singleline(input)
                            .hint_text("8-digit code")
                            .font(egui::TextStyle::Monospace)
                            .char_limit(12)
                            .desired_width(200.),
                    );

                    // Also works with touch or Steam Input when the approving device is a Deck.
                    egui::Grid::new("device-digits").show(ui, |ui| {
                        for label in [
                            "1", "2", "3", "4", "5", "6", "7", "8", "9", "Clear", "0", "Delete",
                        ] {
                            if ui.add_sized([58., 38.], egui::Button::new(label)).clicked() {
                                match label {
                                    "Clear" => input.clear(),
                                    "Delete" => {
                                        input.pop();
                                    }
                                    _ if input.chars().filter(char::is_ascii_digit).count() < 8 => {
                                        input.push_str(label)
                                    }
                                    _ => {}
                                }
                            }
                            if matches!(label, "3" | "6" | "9" | "Delete") {
                                ui.end_row();
                            }
                        }
                    });
                    let code = device_code::normalize(input);
                    ui.horizontal(|ui| {
                        let cancel = ui.add(phase::quiet("Cancel")).clicked();
                        if ui
                            .add_enabled(code.is_some(), phase::primary("Review device"))
                            .clicked()
                            || field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter))
                        {
                            lookup = code;
                        }
                        cancel
                    })
                    .inner
                }
                Some(DeviceDialog::Approve {
                    review,
                    scope,
                    account,
                    server,
                }) => {
                    ui.strong(&review.device);
                    ui.label(format!("Sign in to {account} on {server}?"));
                    ui.horizontal(|ui| {
                        let cancel = ui.add(phase::quiet("Cancel")).clicked();
                        if ui.add(phase::primary("Approve")).clicked() {
                            approval = Some((review.clone(), scope.clone()));
                        }
                        cancel
                    })
                    .inner
                }
                _ => false,
            })
            .inner
        });
        if !self.busy && (response.inner || response.should_close()) {
            self.device_dialog = None;
        } else if let Some(code) = lookup {
            let account = self.account.clone();
            let server = self.url.clone();
            self.job(move |l| {
                let c = l.credentials()?;
                ensure!(
                    account == c.account && server == c.url,
                    "Account changed. Open the device code again."
                );
                let review = device_code::review(&c, &code)?;
                Ok(Event::DeviceDialog(DeviceDialog::Approve {
                    review,
                    scope: l.scope(&c),
                    account,
                    server,
                }))
            });
        } else if let Some((review, scope)) = approval {
            self.device_dialog = None;
            self.job(move |l| {
                let c = l.credentials()?;
                ensure!(
                    scope == l.scope(&c),
                    "Account changed. Review the device again."
                );
                device_code::approve(&c, &review)?;
                Ok(Event::Notice(format!(
                    "Approved {}. Finish signing in on that device.",
                    review.device
                )))
            });
        }
    }
}

pub(crate) fn approve_terminal(local: &crate::local::Local, code: &str) -> Result<()> {
    let c = local.credentials()?;
    let review = device_code::review(&c, code)?;
    let question = format!("Sign {} in as {} on {}?", review.device, c.account, c.url);
    if inquire::Confirm::new(&question)
        .with_default(false)
        .prompt()?
    {
        let active = local.credentials()?;
        ensure!(
            local.scope(&active) == local.scope(&c),
            "Account changed. Review the device again."
        );
        device_code::approve(&active, &review)?;
        println!("Approved. Finish signing in on {}.", review.device);
    }
    Ok(())
}

pub(crate) fn device_login_terminal(
    local: &crate::local::Local,
    url: &str,
    pairing: Option<&crate::host::Pairing>,
    device: String,
    private: bool,
) -> Result<()> {
    let pending = Pending::start(url, pairing, device)?;
    println!(
        "Choose Approve a device in Settings on your signed-in device.\n{}",
        pending.display_code()
    );
    let result = (|| {
        loop {
            thread::sleep(Duration::from_secs(3));
            ensure!(
                Instant::now() < pending.deadline,
                "Code expired. Start device sign-in again."
            );
            let Some(account) = pending.poll()? else {
                continue;
            };
            if inquire::Confirm::new(&format!("Sign in as {account} on {url}?"))
                .with_default(false)
                .prompt()?
            {
                let c = pending.finish(&account)?;
                if let Err(error) = local.set_login(&c, private) {
                    if private || !error.is::<keyring::Error>() {
                        return Err(error);
                    }
                    if !inquire::Confirm::new("Credential store unavailable. Save this sign-in in Buddy’s private app data?")
                        .with_default(false).prompt()? {
                        return Ok(());
                    }
                    local.set_login(&c, true)?;
                }
                println!("Signed in as {account}.");
            }
            return Ok(());
        }
    })();
    let _ = pending.cancel();
    result
}
