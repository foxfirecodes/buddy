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
    Opening,
    Welcome,
    Server,
    Account {
        private_only: bool,
    },
    Storage {
        credentials: Credentials,
        private_only: bool,
        error: anyhow::Error,
    },
    SavedSignIn(Credentials, SavedTokenIssue),
    ProfileError(anyhow::Error),
}

pub(super) enum SavedTokenIssue {
    NoService,
    NoEntry,
    Inaccessible(anyhow::Error),
}

// Only a successful inventory can rule out a store. A locked wallet may still own
// the name, and an idle provider can be activated when the user actually signs in.
#[cfg(target_os = "linux")]
async fn no_secret_service(connection: &zbus::Connection) -> anyhow::Result<bool> {
    let bus = zbus::fdo::DBusProxy::new(connection).await?;
    let name = "org.freedesktop.secrets";
    if bus.name_has_owner(name.try_into()?).await? {
        return Ok(false);
    }
    let activatable = bus.list_activatable_names().await?;
    Ok(!activatable.iter().any(|item| item.as_str() == name))
}

pub(super) fn secret_service_absent() -> bool {
    #[cfg(target_os = "linux")]
    {
        use std::time::Duration;
        async fn probe() -> anyhow::Result<bool> {
            let connection = zbus::Connection::session().await?;
            no_secret_service(&connection).await
        }
        let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
        else {
            return false;
        };
        runtime.block_on(async {
            matches!(
                tokio::time::timeout(Duration::from_secs(1), probe()).await,
                Ok(Ok(true))
            )
        })
    }
    #[cfg(not(target_os = "linux"))]
    false
}

fn saved_sign_in(local: &Local) -> Event {
    let record = match local.read::<Credentials>("login") {
        Ok(Some(record)) => record,
        Ok(None) => return Event::Onboarding(JoinStep::Welcome),
        Err(error) => return Event::Onboarding(JoinStep::ProfileError(error)),
    };
    if !record.token.is_empty() {
        return Event::Signed(record);
    }
    let issue = if secret_service_absent() {
        SavedTokenIssue::NoService
    } else {
        match local.unlock_credential(record.clone()) {
            Ok(credentials) => return Event::Signed(credentials),
            Err(error) if matches!(error.downcast_ref(), Some(keyring::Error::NoEntry)) => {
                SavedTokenIssue::NoEntry
            }
            Err(error) => SavedTokenIssue::Inaccessible(error),
        }
    };
    Event::Onboarding(JoinStep::SavedSignIn(record, issue))
}

fn save_sign_in(
    local: &Local,
    credentials: Credentials,
    private_file: bool,
) -> anyhow::Result<Event> {
    match local.set_login(&credentials, private_file) {
        Ok(()) => Ok(Event::Signed(credentials)),
        Err(error) if !private_file && error.is::<keyring::Error>() => {
            Ok(Event::Onboarding(JoinStep::Storage {
                credentials,
                private_only: secret_service_absent(),
                error,
            }))
        }
        Err(error) => Err(error),
    }
}

impl App {
    pub(super) fn open_profile(&mut self) {
        self.join_step = JoinStep::Opening;
        self.job(|local| Ok(saved_sign_in(&local)));
    }

    fn open_account_form(&mut self) {
        self.job(|_| {
            Ok(Event::Onboarding(JoinStep::Account {
                private_only: secret_service_absent(),
            }))
        });
    }

    pub(super) fn remember_profile(&mut self, credentials: &Credentials) {
        self.url = credentials.url.clone();
        self.account = credentials.account.clone();
        self.device = credentials.device.clone();
        self.pairing = host::Pairing::remembered(credentials);
    }

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
            self.show_error(error);
            return false;
        }
        true
    }

    pub(super) fn login(&mut self, ui: &mut egui::Ui) {
        ui.set_width(ui.available_width());
        match self.join_step {
            JoinStep::Opening => {
                ui.label("Opening profile…");
            }
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
            JoinStep::Account { private_only } => self.account_form(ui, private_only),
            JoinStep::Storage { private_only, .. } => self.credential_storage(ui, private_only),
            JoinStep::SavedSignIn(_, _) => self.recover_saved_sign_in(ui),
            JoinStep::ProfileError(ref error) => {
                phase_heading(ui, "Couldn’t read profile");
                ui.label("Check that Buddy can read its app data folder.");
                ui.label(self.local.home.display().to_string());
                ui.collapsing("Details", |ui| ui.label(format!("{error:#}")));
                if ui.button("Retry").clicked() {
                    self.open_profile();
                }
            }
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
            let address = local::canonical_url(&self.url);
            if !self.url.trim().is_empty()
                && let Err(error) = &address
            {
                ui.colored_label(ui.visuals().error_fg_color, error.to_string());
            }
            if ui
                .add_enabled(address.is_ok(), egui::Button::new("Continue"))
                .clicked()
                && let Ok(url) = address
            {
                self.url = url;
                self.pairing = None;
                self.open_account_form();
            }
        });
    }

    fn import_pairing(&mut self, path: &Path) {
        match host::Pairing::load(path) {
            Ok(pair) => {
                self.url = pair.url.clone();
                self.pairing = Some(pair);
                self.open_account_form();
            }
            Err(error) => {
                self.show_error(error);
            }
        }
    }

    fn account_form(&mut self, ui: &mut egui::Ui, private_only: bool) {
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
        if private_only {
            ui.label("Your device token will be saved in Buddy’s private app data.");
        } else {
            ui.collapsing("Credential storage", |ui| {
                ui.checkbox(
                    &mut self.private_file,
                    "Store token in private app data on this device",
                );
            });
        }
        let ready = !self.account.trim().is_empty()
            && !self.password.is_empty()
            && !self.device.trim().is_empty()
            && (!self.register || !self.invite.trim().is_empty());
        let action = match (self.register, private_only) {
            (false, true) => "Sign in and save token",
            (true, true) => "Create account and save token",
            (false, false) => "Sign in",
            (true, false) => "Create account",
        };
        if ui.add_enabled(ready, phase::primary(action)).clicked() {
            let url = self.url.clone();
            let login = Login {
                account: self.account.clone(),
                password: std::mem::take(&mut self.password),
                device: self.device.clone(),
                invite: self.register.then(|| std::mem::take(&mut self.invite)),
            };
            let private_file = private_only || self.private_file;
            let pairing = self.pairing.clone();
            self.job(move |l| {
                let credentials = Api::login_paired(&url, login, pairing.as_ref())?;
                save_sign_in(&l, credentials, private_file)
            });
        }
    }

    fn recover_saved_sign_in(&mut self, ui: &mut egui::Ui) {
        let JoinStep::SavedSignIn(record, issue) = &self.join_step else {
            return;
        };
        phase_heading(ui, "Saved sign-in");
        ui.strong(&record.account);
        ui.weak(&record.url);
        match issue {
            SavedTokenIssue::NoService => {
                ui.label("The system credential store isn’t available in this session. Sign in again to use private app data.");
            }
            SavedTokenIssue::NoEntry => {
                ui.label("This device’s token is missing from the system credential store.");
            }
            SavedTokenIssue::Inaccessible(error) => {
                ui.label("Buddy couldn’t read this device’s token. Unlock the system credential store to continue.");
                ui.collapsing("Details", |ui| ui.label(format!("{error:#}")));
                if ui.add(phase::primary("Retry after unlocking")).clicked() {
                    self.open_profile();
                    return;
                }
            }
        }
        if ui.button("Sign in again").clicked() {
            self.register = false;
            self.open_account_form();
        }
    }

    fn credential_storage(&mut self, ui: &mut egui::Ui, private_only: bool) {
        phase_heading(ui, "Credential storage");
        ui.label("The system credential store is unavailable. Save this device’s sign-in token in Buddy’s private app data?");
        ui.add_space(8.);
        ui.horizontal_wrapped(|ui| {
            let private = ui.add(phase::primary("Use private app data")).clicked();
            let retry = !private_only && ui.button("Retry credential store").clicked();
            if (private || retry)
                && let JoinStep::Storage { credentials, .. } = &self.join_step
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
                self.error = None;
                self.register = false;
                self.join_step = JoinStep::Account { private_only };
            }
        });
        if let JoinStep::Storage { error, .. } = &self.join_step {
            ui.collapsing("Details", |ui| ui.monospace(format!("{error:#}")));
        }
    }

    pub(super) fn switch_account(&mut self) {
        // Keep the current server's trust identity for a different account on that server.
        self.account.clear();
        self.password.clear();
        self.invite.clear();
        self.register = false;
        self.note.clear();
        self.open_account_form();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[tokio::test]
    #[ignore = "requires dbus-daemon and a private session socket"]
    async fn probe_recognizes_running_and_activatable_secret_services() -> anyhow::Result<()> {
        use std::{
            fs,
            io::{BufRead, BufReader},
            process::{Child, Stdio},
        };

        struct Bus(Child);
        impl Drop for Bus {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }

        let dir = tempfile::tempdir()?;
        let config = dir.path().join("bus.conf");
        fs::write(
            &config,
            format!(
                r#"<busconfig><type>session</type>
            <listen>unix:tmpdir={dir}</listen><servicedir>{dir}</servicedir>
            <policy context="default"><allow own="*"/><allow send_destination="*"/>
            <allow receive_sender="*"/></policy></busconfig>"#,
                dir = dir.path().display()
            ),
        )?;
        let mut daemon = Bus(Command::new("dbus-daemon")
            .arg("--config-file")
            .arg(config)
            .args(["--nofork", "--print-address=1"])
            .stdout(Stdio::piped())
            .spawn()?);
        let mut address = String::new();
        BufReader::new(daemon.0.stdout.take().unwrap()).read_line(&mut address)?;
        let connection = zbus::connection::Builder::address(address.trim())?
            .build()
            .await?;
        assert!(no_secret_service(&connection).await?);

        let name = "org.freedesktop.secrets";
        connection.request_name(name).await?;
        assert!(!no_secret_service(&connection).await?);
        connection.release_name(name).await?;
        fs::write(
            dir.path().join(format!("{name}.service")),
            format!("[D-BUS Service]\nName={name}\nExec=/bin/false\n"),
        )?;
        let bus = zbus::fdo::DBusProxy::new(&connection).await?;
        bus.reload_config().await?;
        assert!(!no_secret_service(&connection).await?);
        assert!(!bus.name_has_owner(name.try_into()?).await?);
        Ok(())
    }

    #[test]
    fn an_unreadable_saved_profile_is_not_first_run() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let local = Local::open(Some(dir.path().join("profile")))?;
        assert!(matches!(
            saved_sign_in(&local),
            Event::Onboarding(JoinStep::Welcome)
        ));
        local.write("login", &"damaged record")?;
        assert!(matches!(
            saved_sign_in(&local),
            Event::Onboarding(JoinStep::ProfileError(_))
        ));
        assert_eq!(
            local.read::<String>("login")?.as_deref(),
            Some("damaged record")
        );
        Ok(())
    }

    #[test]
    fn consenting_to_private_storage_persists_the_paired_device() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let local = Local::open(Some(dir.path().join("profile")))?;
        let device = Credentials {
            url: "https://example.invalid".into(),
            account: "player".into(),
            device: "game-room".into(),
            token: "issued-token".into(),
            paired_ca: Some("trusted server certificate".into()),
        };
        assert!(matches!(
            save_sign_in(&local, device.clone(), true)?,
            Event::Signed(_)
        ));
        let Event::Signed(saved) = saved_sign_in(&local) else {
            panic!("expected a signed-in device");
        };
        assert_eq!(
            (
                saved.account,
                saved.device,
                saved.url,
                saved.token,
                saved.paired_ca
            ),
            (
                device.account,
                device.device,
                device.url,
                device.token,
                device.paired_ca
            )
        );
        Ok(())
    }
}
