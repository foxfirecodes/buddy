use crate::{
    local::{Local, canonical_url},
    server, ui,
};
use anyhow::{Context, Result, ensure};
use eframe::egui;
use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, DnType, ExtendedKeyUsagePurpose, IsCa,
    KeyPair, KeyUsagePurpose,
};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{Read, Write},
    net::{IpAddr, SocketAddr, UdpSocket},
    path::{Path, PathBuf},
    sync::mpsc,
    thread,
    time::Duration,
};

const PAIR_FILE: &str = "buddy-pair.json";
const CONFIG: &str = "server.toml";
pub fn pair_path(local: &Local) -> PathBuf {
    local.home.join("hosted-server").join(PAIR_FILE)
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pairing {
    version: u8,
    pub url: String,
    pub ca: String,
}
impl Pairing {
    pub fn remembered(c: &crate::model::Credentials) -> Option<Self> {
        Some(Self {
            version: 1,
            url: c.url.clone(),
            ca: c.paired_ca.clone()?,
        })
    }
    pub fn load(path: &Path) -> Result<Self> {
        let mut bytes = Vec::new();
        fs::File::open(path)?.take(16385).read_to_end(&mut bytes)?;
        ensure!(bytes.len() <= 16384, "pairing file exceeds 16 KiB");
        let mut pair: Self =
            serde_json::from_slice(&bytes).context("read Buddy server pairing file")?;
        pair.url = canonical_url(&pair.url)?;
        pair.validate()?;
        Ok(pair)
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(self.version == 1, "unsupported pairing file version");
        let url = reqwest::Url::parse(&self.url)?;
        ensure!(
            canonical_url(&self.url)? == self.url && url.scheme() == "https" && url.path() == "/",
            "pairing needs an HTTPS server address"
        );
        use rustls::pki_types::{CertificateDer, pem::PemObject};
        let mut certs = CertificateDer::pem_slice_iter(self.ca.as_bytes());
        let cert = certs.next().context("pairing file needs a certificate")??;
        ensure!(
            certs.next().is_none(),
            "pairing file must contain exactly one host CA"
        );
        rustls::RootCertStore::empty()
            .add(cert)
            .context("invalid server CA")?;
        Ok(())
    }
    pub fn fingerprint(&self) -> String {
        // Display the same identity on the host and clients, including across CRLF conversions.
        let hash = crate::model::digest(self.ca.replace('\r', "").as_bytes());
        hash.as_bytes()[..32]
            .chunks(4)
            .map(|part| std::str::from_utf8(part).unwrap())
            .collect::<Vec<_>>()
            .join(" ")
    }
}
fn suggested_ip() -> IpAddr {
    // A UDP connect resolves the kernel route without sending a packet.
    UdpSocket::bind("0.0.0.0:0")
        .and_then(|socket| {
            socket.connect("192.0.2.1:9")?;
            socket.local_addr()
        })
        .map(|addr| addr.ip())
        .unwrap_or_else(|_| std::net::Ipv4Addr::LOCALHOST.into())
}
#[cfg(unix)]
fn protect_dir(dir: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    Ok(())
}
#[cfg(windows)]
fn current_user_sid() -> Result<String> {
    use std::{
        io,
        mem::size_of,
        os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
        ptr, slice,
    };
    use windows_sys::Win32::{
        Foundation::LocalFree,
        Security::{
            Authorization::ConvertSidToStringSidW, GetTokenInformation, TOKEN_QUERY, TOKEN_USER,
            TokenUser,
        },
        System::Threading::{GetCurrentProcess, OpenProcessToken},
    };
    // Read the current process token. The OS supplies the TOKEN_USER buffer size and SID.
    unsafe {
        let mut raw = ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw) == 0 {
            return Err(io::Error::last_os_error().into());
        }
        let token = OwnedHandle::from_raw_handle(raw);
        let mut needed = 0;
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            ptr::null_mut(),
            0,
            &mut needed,
        );
        let mut buffer = vec![0usize; (needed as usize).div_ceil(size_of::<usize>())];
        if needed == 0
            || GetTokenInformation(
                token.as_raw_handle(),
                TokenUser,
                buffer.as_mut_ptr().cast(),
                needed,
                &mut needed,
            ) == 0
        {
            return Err(io::Error::last_os_error().into());
        }
        let user = &*buffer.as_ptr().cast::<TOKEN_USER>();
        let mut sid = ptr::null_mut();
        if ConvertSidToStringSidW(user.User.Sid, &mut sid) == 0 {
            return Err(io::Error::last_os_error().into());
        }
        let mut len = 0;
        while *sid.add(len) != 0 {
            len += 1;
        }
        let text = String::from_utf16(slice::from_raw_parts(sid, len));
        LocalFree(sid.cast());
        Ok(text?)
    }
}
#[cfg(windows)]
fn protect_dir(dir: &Path) -> Result<()> {
    use std::{io, os::windows::ffi::OsStrExt, ptr};
    use windows_sys::Win32::{
        Foundation::LocalFree,
        Security::{
            Authorization::{
                ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
            },
            DACL_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, SetFileSecurityW,
        },
    };
    // The signed-in account and SYSTEM inherit full access, including in an elevated session.
    let acl: Vec<u16> = format!("D:P(A;OICI;FA;;;{})(A;OICI;FA;;;SY)", current_user_sid()?)
        .encode_utf16()
        .chain([0])
        .collect();
    let path: Vec<u16> = dir.as_os_str().encode_wide().chain([0]).collect();
    let mut descriptor = ptr::null_mut();
    // Both strings are nul-terminated and the allocated descriptor stays alive through SetFileSecurityW.
    unsafe {
        if ConvertStringSecurityDescriptorToSecurityDescriptorW(
            acl.as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            ptr::null_mut(),
        ) == 0
        {
            return Err(io::Error::last_os_error().into());
        }
        let set = SetFileSecurityW(
            path.as_ptr(),
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            descriptor,
        );
        let result = if set == 0 {
            Err(io::Error::last_os_error().into())
        } else {
            Ok(())
        };
        LocalFree(descriptor);
        result
    }
}
fn initialize(dir: &Path, addr: SocketAddr) -> Result<Pairing> {
    ensure!(
        addr.port() != 0 && !addr.ip().is_unspecified() && !addr.ip().is_multicast(),
        "choose this computer's network address and a port from 1 to 65535"
    );
    let url = canonical_url(&format!("https://{addr}"))?;
    if dir.exists() {
        let pair = Pairing::load(&dir.join(PAIR_FILE))?;
        ensure!(
            pair.url == url,
            "this profile already hosts {}. Start it using that address",
            pair.url
        );
        return Ok(pair);
    }
    let tmp = tempfile::tempdir_in(
        dir.parent()
            .context("server data directory needs a parent")?,
    )?;
    protect_dir(tmp.path()).context("create private host storage")?;
    let mut ca_params = CertificateParams::default();
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    ca_params
        .distinguished_name
        .push(DnType::CommonName, "Buddy private server CA");
    ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign];
    let ca = CertifiedIssuer::self_signed(ca_params, KeyPair::generate()?)?;
    let mut leaf = CertificateParams::new(vec![addr.ip().to_string()])?;
    leaf.distinguished_name.push(DnType::CommonName, "Buddy");
    leaf.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    leaf.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let key = KeyPair::generate()?;
    let certificate = leaf.signed_by(&key, &ca)?;
    let pair = Pairing {
        version: 1,
        url,
        ca: ca.pem(),
    };
    // Only the leaf key persists. The exported file contains public material.
    let mut private = fs::File::options();
    private.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        private.mode(0o600);
    }
    private
        .open(tmp.path().join("server-key.pem"))?
        .write_all(key.serialize_pem().as_bytes())?;
    fs::write(tmp.path().join("server-cert.pem"), certificate.pem())?;
    fs::write(
        tmp.path().join(PAIR_FILE),
        serde_json::to_vec_pretty(&pair)?,
    )?;
    fs::write(
        tmp.path().join(CONFIG),
        format!(
            "listen = \"{addr}\"\ndatabase = \"server.sqlite3\"\n\n[tls]\ncertificate = \"server-cert.pem\"\nprivate_key = \"server-key.pem\"\n"
        ),
    )?;
    fs::rename(tmp.path(), dir).context("install server settings")?;
    Ok(pair)
}
enum HostEvent {
    Ready(Pairing),
    Finished(Result<(), String>),
}
struct Host {
    dir: PathBuf,
    ip: String,
    port: String,
    tx: mpsc::Sender<HostEvent>,
    rx: mpsc::Receiver<HostEvent>,
    handle: Option<axum_server::Handle<SocketAddr>>,
    pair: Option<Pairing>,
    invite: String,
    notice: String,
}
impl Host {
    fn new(local: Local) -> Result<Self> {
        let dir = local.home.join("hosted-server");
        let addr = if dir.exists() {
            let pair = Pairing::load(&dir.join(PAIR_FILE))?;
            let url = reqwest::Url::parse(&pair.url)?;
            let ip = url
                .host_str()
                .context("server IP missing")?
                .trim_matches(['[', ']'])
                .parse::<IpAddr>()
                .context("stored host needs an IP address")?;
            SocketAddr::new(
                ip,
                url.port_or_known_default().context("server port missing")?,
            )
        } else {
            SocketAddr::new(suggested_ip(), 7341)
        };
        let (tx, rx) = mpsc::channel();
        Ok(Self {
            dir,
            ip: addr.ip().to_string(),
            port: addr.port().to_string(),
            tx,
            rx,
            handle: None,
            pair: None,
            invite: String::new(),
            notice: String::new(),
        })
    }
    fn start(&mut self) -> Result<()> {
        let ip = self
            .ip
            .trim()
            .parse()
            .context("enter an IPv4 or IPv6 address")?;
        let port = self
            .port
            .trim()
            .parse()
            .context("enter a port from 1 to 65535")?;
        let addr = SocketAddr::new(ip, port);
        // Reject an unusable address before persisting its server identity.
        drop(
            std::net::TcpListener::bind(addr)
                .context("could not listen on that address and port")?,
        );
        let pair = initialize(&self.dir, addr)?;
        let handle = axum_server::Handle::new();
        self.handle = Some(handle.clone());
        self.notice.clear();
        let config = self.dir.join(CONFIG);
        let events = self.tx.clone();
        thread::spawn(move || {
            let result = server::serve_controlled(&config, handle, |_| {
                let _ = events.send(HostEvent::Ready(pair));
            });
            let _ = events.send(HostEvent::Finished(result.map_err(|e| format!("{e:#}"))));
        });
        Ok(())
    }
    fn poll(&mut self) {
        for event in self.rx.try_iter() {
            match event {
                HostEvent::Ready(p) => self.pair = Some(p),
                HostEvent::Finished(result) => {
                    self.pair = None;
                    self.handle = None;
                    self.invite.clear();
                    self.notice = result.err().unwrap_or_else(|| "Server stopped.".into());
                }
            }
        }
    }
    fn export(&self, pair: &Pairing, path: &Path) -> Result<()> {
        let path = std::path::absolute(path)?;
        let parent = path
            .parent()
            .context("choose a destination folder")?
            .canonicalize()?;
        ensure!(
            !parent.starts_with(&self.dir),
            "export outside the server data folder"
        );
        let mut temp = tempfile::NamedTempFile::new_in(parent)?;
        serde_json::to_writer_pretty(&mut temp, pair)?;
        temp.as_file().sync_all()?;
        temp.persist_noclobber(path)
            .context("choose a new pairing filename")?;
        Ok(())
    }
    fn window(&mut self, ui: &mut egui::Ui) {
        ui.heading("Host a Buddy server");
        ui.label(self.dir.display().to_string());
        ui.add_space(8.);
        if let Some(p) = &self.pair {
            ui.label(format!("Listening at {}", p.url));
            ui.label("Keep this window open while your devices sync.");
            ui.separator();
            ui.label("On each device, open the pairing file in Buddy and check this fingerprint.");
            ui.monospace(p.fingerprint());
            if ui.button("Export pairing file…").clicked()
                && let Some(path) = rfd::FileDialog::new().set_file_name(PAIR_FILE).save_file()
            {
                self.notice = match self.export(p, &path) {
                    Ok(()) => "Pairing file exported.".into(),
                    Err(e) => format!("{e:#}"),
                };
            }
            if ui.button("Create account invite").clicked() {
                match server::invite(&self.dir.join(CONFIG)) {
                    Ok(code) => {
                        self.invite = code;
                        self.notice.clear();
                    }
                    Err(e) => self.notice = format!("{e:#}"),
                }
            }
            if !self.invite.is_empty() {
                ui.label("This invite creates one account and expires in 24 hours.");
                if ui.button("Copy invite").clicked() {
                    ui.ctx().copy_text(self.invite.clone());
                }
                ui.monospace(&self.invite);
            }
            ui.separator();
            if ui.button("Stop server").clicked()
                && let Some(handle) = &self.handle
            {
                handle.graceful_shutdown(Some(Duration::from_secs(10)));
            }
        } else {
            ui.label("Choose the IP address this computer uses on your home network.");
            ui.add_enabled_ui(self.handle.is_none() && !self.dir.exists(), |ui| {
                ui.label("Host IP address");
                ui.text_edit_singleline(&mut self.ip);
                ui.label("Port");
                ui.text_edit_singleline(&mut self.port);
            });
            if ui
                .add_enabled(
                    self.handle.is_none(),
                    egui::Button::new("Start HTTPS server"),
                )
                .clicked()
                && let Err(e) = self.start()
            {
                self.notice = format!("{e:#}");
            }
            if self.handle.is_some() {
                ui.spinner();
            }
        }
        if !self.notice.is_empty() {
            ui.label(&self.notice);
        }
    }
}
pub fn gui(local: Local) -> Result<()> {
    let mut host = Host::new(local)?;
    let mut first = true;
    eframe::run_simple_native(
        "Host a Buddy server",
        ui::native([650., 560.]),
        move |ctx, _| {
            if first {
                ui::style(ctx);
                first = false;
            }
            host.poll();
            egui::CentralPanel::default().show(ctx, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| host.window(ui));
            });
            if host.handle.is_some() {
                ctx.request_repaint_after(Duration::from_millis(150));
            }
        },
    )
    .map_err(|e| anyhow::anyhow!(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        local::Api,
        model::{Action, GameInfo, Login},
    };
    #[test]
    fn paired_server_authenticates_the_endpoint_and_survives_restart() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let dir = tmp.path().join("host");
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let addr = listener.local_addr()?;
        drop(listener);
        let pair = initialize(&dir, addr)?;
        let again = initialize(&dir, addr)?;
        assert_eq!(again.fingerprint(), pair.fingerprint());
        let serial = fs::read_to_string(dir.join(PAIR_FILE))?;
        assert!(!serial.contains("PRIVATE KEY"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(dir.join("server-key.pem"))?
                    .permissions()
                    .mode()
                    & 0o077,
                0
            );
            assert_eq!(fs::metadata(&dir)?.permissions().mode() & 0o077, 0);
        }
        let cfg = dir.join(CONFIG);
        let invite = server::invite(&cfg)?;
        let handle = axum_server::Handle::new();
        let stop = handle.clone();
        let (ready, rx) = mpsc::channel();
        let task = thread::spawn(move || {
            server::serve_controlled(&cfg, handle, |_| {
                let _ = ready.send(());
            })
        });
        let result = (|| -> Result<()> {
            rx.recv_timeout(Duration::from_secs(10))?;
            let login = Login {
                device: "laptop".into(),
                account: "tester".into(),
                password: "disposable-test-password".into(),
                invite: Some(invite),
            };
            let creds = Api::login_paired(&pair.url, login.clone(), Some(&pair))?;
            let local = Local::open(Some(tmp.path().join("client")))?;
            local.set_login(&creds, true)?;
            let remembered = local.credentials()?;
            let games: Vec<GameInfo> = Api::new(remembered.clone())?.rpc(Action::Games)?;
            assert!(games.is_empty());
            let login = Login {
                invite: None,
                ..login
            };
            assert!(Api::login_paired(&pair.url, login.clone(), Some(&pair)).is_ok());
            let other = initialize(&tmp.path().join("other-host"), addr)?;
            assert_ne!(pair.fingerprint(), other.fingerprint());
            assert!(Api::login_paired(&pair.url, login.clone(), Some(&other)).is_err());
            assert!(Api::login_paired(&pair.url, login.clone(), None).is_err());
            let mut wrong = pair.clone();
            wrong.url = format!("https://localhost:{}", addr.port());
            assert!(Api::login_paired(&wrong.url, login.clone(), Some(&wrong)).is_err());
            assert!(Api::login_paired(&wrong.url, login.clone(), Some(&pair)).is_err());
            wrong.url = format!("http://{addr}");
            assert!(Api::login_paired(&wrong.url, login, Some(&wrong)).is_err());
            let mut legacy = serde_json::to_value(&remembered)?;
            legacy.as_object_mut().unwrap().remove("paired_ca");
            let legacy = serde_json::from_value(legacy)?;
            assert_ne!(local.scope(&legacy), local.scope(&remembered));
            assert!(
                Api::new(legacy)?
                    .rpc::<Vec<GameInfo>>(Action::Games)
                    .is_err()
            );
            Ok(())
        })();
        stop.graceful_shutdown(Some(Duration::from_secs(1)));
        task.join().expect("server thread")?;
        result
    }
    #[test]
    fn remembers_https_default_port_and_ipv6_hosts() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        for (i, (socket, expected)) in [
            ("127.0.0.1:443", "https://127.0.0.1"),
            ("[::1]:443", "https://[::1]"),
            ("[::1]:7443", "https://[::1]:7443"),
        ]
        .into_iter()
        .enumerate()
        {
            let profile = Local::open(Some(tmp.path().join(i.to_string())))?;
            let dir = profile.home.join("hosted-server");
            let addr: SocketAddr = socket.parse()?;
            let pair = initialize(&dir, addr)?;
            assert_eq!(pair.url, expected);
            assert_eq!(Pairing::load(&dir.join(PAIR_FILE))?.url, expected);
            assert_eq!(initialize(&dir, addr)?.url, expected);
            let host = Host::new(profile)?;
            assert_eq!(host.port, addr.port().to_string());
            assert_eq!(host.ip, addr.ip().to_string());
        }
        Ok(())
    }
    #[test]
    fn rejects_malformed_pairings_and_keeps_existing_storage() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let dir = tmp.path().join("host");
        fs::create_dir(&dir)?;
        fs::write(
            dir.join(PAIR_FILE),
            br#"{"version":1,"url":"https://localhost","ca":"broken"}"#,
        )?;
        assert!(Pairing::load(&dir.join(PAIR_FILE)).is_err());
        assert!(initialize(&dir, "127.0.0.1:7341".parse()?).is_err());
        assert!(!dir.join("server-key.pem").exists());
        fs::write(dir.join(PAIR_FILE), vec![0u8; 16385])?;
        assert!(Pairing::load(&dir.join(PAIR_FILE)).is_err());
        Ok(())
    }
}
