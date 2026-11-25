use super::{App, Event, phase_heading};
use crate::{
    host,
    local::{self, Api, Local},
    model::{Credentials, Login},
    phase,
};
use eframe::egui;
use std::{path::Path, process::Command};

pub(super) enum JoinStep {
    Welcome,
    Server,
    Account,
    Storage(Credentials),
}

fn save_sign_in(
    local: &Local,
    credentials: Credentials,
    private_file: bool,
) -> anyhow::Result<Event> {
    match local.set_login(&credentials, private_file) {
        Ok(()) => Ok(Event::Signed),
        Err(error) if !private_file && error.is::<keyring::Error>() => {
            Ok(Event::ChooseCredentialStorage(credentials))
        }
        Err(error) => Err(error),
    }
}

impl App {
    pub(super) fn launch_host(&mut self) -> bool {
        let opened = (|| -> anyhow::Result<()> {
            let mut cmd = Command::new(std::env::current_exe()?);
            if self.local.explicit_home {
                cmd.arg("--profile-dir").arg(&self.local.home);
            }
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;
                cmd.creation_flags(CREATE_NO_WINDOW);
            }
            cmd.arg("host").spawn()?;
            Ok(())
        })();
        if let Err(error) = opened {
            self.error = true;
            self.note = format!("{error:#}");
            return false;
        }
        true
    }

    pub(super) fn login(&mut self, ui: &mut egui::Ui) {
        ui.set_width(ui.available_width());
        match self.join_step {
            JoinStep::Welcome => {
                phase_heading(ui, "Set up Buddy");
                ui.add_space(14.);
                let size = egui::vec2(ui.available_width().min(430.), 48.);
                if ui
                    .add_sized(size, phase::primary("Join a server"))
                    .clicked()
                {
                    self.join_step = JoinStep::Server;
                }
                if ui
                    .add_sized(size, egui::Button::new("Host a server"))
                    .clicked()
                    && self.launch_host()
                {
                    self.join_step = JoinStep::Server;
                }
            }
            JoinStep::Server => self.join_server(ui),
            JoinStep::Account => self.account_form(ui),
            JoinStep::Storage(_) => self.credential_storage(ui),
        }
    }

    fn join_server(&mut self, ui: &mut egui::Ui) {
        if ui.button("Back").clicked() {
            self.note.clear();
            self.join_step = JoinStep::Welcome;
            return;
        }
        phase_heading(ui, "Join a server");
        ui.add_space(10.);
        if ui.add(phase::primary("Open pairing file…")).clicked()
            && let Some(file) = rfd::FileDialog::new()
                .add_filter("Buddy pairing", &["json"])
                .pick_file()
        {
            self.import_pairing(&file);
        }
        let own = host::pair_path(&self.local);
        if own.is_file() && ui.button("Use my hosted server").clicked() {
            self.import_pairing(&own);
        }
        ui.add_space(4.);
        ui.collapsing("Enter server address", |ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.url)
                    .hint_text("https://server.example")
                    .desired_width(f32::INFINITY),
            );
            if ui.button("Continue").clicked() {
                match local::canonical_url(&self.url) {
                    Ok(url) => {
                        self.url = url;
                        self.pairing = None;
                        self.join_step = JoinStep::Account;
                        self.note.clear();
                        self.error = false;
                    }
                    Err(error) => {
                        self.note = format!("{error:#}");
                        self.error = true;
                    }
                }
            }
        });
    }

    fn import_pairing(&mut self, path: &Path) {
        match host::Pairing::load(path) {
            Ok(pair) => {
                self.url = pair.url.clone();
                self.pairing = Some(pair);
                self.join_step = JoinStep::Account;
                self.error = false;
                self.note.clear();
            }
            Err(error) => {
                self.error = true;
                self.note = format!("{error:#}");
            }
        }
    }

    fn account_form(&mut self, ui: &mut egui::Ui) {
        if ui.button("Change server").clicked() {
            self.password.clear();
            self.invite.clear();
            self.note.clear();
            self.pairing = None;
            self.join_step = JoinStep::Server;
            return;
        }
        phase_heading(
            ui,
            if self.register {
                "Create account"
            } else {
                "Sign in"
            },
        );
        phase::panel().show(ui, |ui| {
            ui.strong(&self.url);
            if let Some(pair) = &self.pairing {
                ui.weak("Compare this fingerprint with the host");
                ui.monospace(pair.fingerprint());
            }
        });
        ui.horizontal_wrapped(|ui| {
            ui.selectable_value(&mut self.register, false, "Existing account");
            ui.selectable_value(&mut self.register, true, "Use an invite");
        });
        ui.label("Account");
        ui.add(egui::TextEdit::singleline(&mut self.account).desired_width(f32::INFINITY));
        ui.label("Password");
        ui.add(
            egui::TextEdit::singleline(&mut self.password)
                .password(true)
                .desired_width(f32::INFINITY),
        );
        ui.label("Device name");
        ui.add(egui::TextEdit::singleline(&mut self.device).desired_width(f32::INFINITY));
        if self.register {
            ui.label("Invite");
            ui.add(egui::TextEdit::singleline(&mut self.invite).desired_width(f32::INFINITY));
        }
        ui.collapsing("Credential storage", |ui| {
            ui.checkbox(
                &mut self.private_file,
                "Store token in private app data on this device",
            );
        });
        let ready = !self.account.trim().is_empty()
            && !self.password.is_empty()
            && !self.device.trim().is_empty()
            && (!self.register || !self.invite.trim().is_empty());
        let action = if self.register {
            "Create account"
        } else {
            "Sign in"
        };
        if ui.add_enabled(ready, phase::primary(action)).clicked() {
            let url = self.url.clone();
            let login = Login {
                account: self.account.clone(),
                password: std::mem::take(&mut self.password),
                device: self.device.clone(),
                invite: self.register.then(|| std::mem::take(&mut self.invite)),
            };
            let private_file = self.private_file;
            let pairing = self.pairing.clone();
            self.job(move |l| {
                let credentials = Api::login_paired(&url, login, pairing.as_ref())?;
                save_sign_in(&l, credentials, private_file)
            });
        }
    }

    fn credential_storage(&mut self, ui: &mut egui::Ui) {
        phase_heading(ui, "Credential storage");
        ui.label("The system credential store is unavailable. Save this device’s sign-in token in Buddy’s private app data?");
        ui.add_space(8.);
        ui.horizontal_wrapped(|ui| {
            let private = ui.add(phase::primary("Use private app data")).clicked();
            let retry = ui.button("Retry credential store").clicked();
            if (private || retry)
                && let JoinStep::Storage(credentials) = &self.join_step
            {
                let credentials = credentials.clone();
                self.job(move |local| save_sign_in(&local, credentials, private));
            }
            if ui.button("Cancel").clicked() {
                self.note = if self.register {
                    "Account created. Sign in to finish on this device."
                } else {
                    "Sign-in wasn’t saved on this device."
                }
                .into();
                self.error = false;
                self.register = false;
                self.join_step = JoinStep::Account;
            }
        });
    }

    pub(super) fn switch_account(&mut self) {
        // Keep the current server's trust identity for a different account on that server.
        self.join_step = JoinStep::Account;
        self.account.clear();
        self.password.clear();
        self.invite.clear();
        self.register = false;
        self.note.clear();
    }
}
