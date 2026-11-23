use crate::{
    files::{Binding, Cache, Restore},
    model::*,
};
use anyhow::{Context, Result, ensure};
use reqwest::blocking::{Client, Response};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    collections::BTreeMap,
    fs,
    net::IpAddr,
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Clone)]
pub struct Local {
    pub home: PathBuf,
    pub explicit_home: bool,
}
#[derive(Default, Clone, Serialize, Deserialize)]
pub struct Launcher {
    pub executable: PathBuf,
    pub args: Vec<String>,
    pub directory: PathBuf,
    pub manual_exit: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Pending {
    pub tree: Tree,
    pub base: Option<Tree>,
    pub time: i64,
    pub op: String,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Setup {
    #[serde(default)]
    pub needs_first_pull: bool,
    #[serde(default)]
    pub bindings_cleared: bool,
    #[serde(default)]
    pub approved_proton_prefix: Option<PathBuf>,
    #[serde(default)]
    pub draft: Option<crate::setup::Draft>,
    pub recipe: Recipe,
    pub revision: i64,
    pub shared: bool,
    pub bindings: BTreeMap<String, Binding>,
    #[serde(default)]
    pub base: Option<Tree>,
    #[serde(default)]
    pub pending: Vec<Pending>,
    #[serde(default)]
    pub restore: Option<Restore>,
    #[serde(default)]
    pub active: bool,
    #[serde(default)]
    pub active_base: Option<Tree>,
    #[serde(default)]
    pub session: Option<String>,
}
impl Setup {
    pub fn new(info: GameInfo) -> Self {
        Self {
            draft: None,
            needs_first_pull: false,
            bindings_cleared: false,
            approved_proton_prefix: None,
            recipe: info.recipe,
            revision: info.revision,
            shared: info.shared,
            bindings: BTreeMap::new(),
            base: None,
            pending: vec![],
            restore: None,
            active: false,
            active_base: None,
            session: None,
        }
    }
}
impl Local {
    pub fn open(profile: Option<PathBuf>) -> Result<Self> {
        let explicit_home = profile.is_some();
        let dir = if let Some(p) = profile {
            p
        } else {
            directories::ProjectDirs::from("org", "Buddy", "Buddy")
                .context("no app data directory")?
                .data_local_dir()
                .to_owned()
        };
        fs::create_dir_all(&dir)?;
        let dir = dir.canonicalize()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            ensure!(
                fs::metadata(&dir)?.permissions().mode() & 0o077 == 0 || {
                    // Only change permissions on the dedicated Buddy directory.
                    let database = dir.join("client.sqlite3");
                    if !database.exists() && fs::read_dir(&dir)?.next().is_none() {
                        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
                        true
                    } else {
                        false
                    }
                },
                "Buddy needs a private data directory (mode 0700): {}",
                dir.display()
            );
        }
        let s = Self {
            home: dir,
            explicit_home,
        };
        let db = s.db()?;
        db.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE IF NOT EXISTS state(k TEXT PRIMARY KEY,v TEXT NOT NULL);")?;
        Ok(s)
    }
    fn db(&self) -> Result<Connection> {
        let c = Connection::open(self.home.join("client.sqlite3"))?;
        c.busy_timeout(Duration::from_secs(10))?;
        c.pragma_update(None, "synchronous", "FULL")?;
        Ok(c)
    }
    fn read<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>> {
        self.db()?
            .query_row("SELECT v FROM state WHERE k=?", [key], |r| {
                r.get::<_, String>(0)
            })
            .optional()?
            .map(|s| serde_json::from_str(&s).map_err(Into::into))
            .transpose()
    }
    fn write(&self, key: &str, v: &impl Serialize) -> Result<()> {
        self.db()?.execute(
            "INSERT INTO state VALUES(?,?) ON CONFLICT(k) DO UPDATE SET v=excluded.v",
            params![key, serde_json::to_string(v)?],
        )?;
        Ok(())
    }
    fn credential_entry(&self, c: &Credentials) -> Result<keyring::Entry> {
        let label =
            digest(format!("{}|{}|{}", self.home.display(), self.scope(c), c.device).as_bytes());
        Ok(keyring::Entry::new("org.buddy.sync", &label)?)
    }
    pub fn credentials(&self) -> Result<Credentials> {
        let mut c: Credentials = self
            .read("login")?
            .context("sign in to a Buddy server first")?;
        if c.token.is_empty() {
            c.token = self
                .credential_entry(&c)?
                .get_password()
                .context("unlock the OS credential store or sign in again")?;
        }
        Ok(c)
    }
    pub fn set_login(&self, c: &Credentials, private_file: bool) -> Result<()> {
        let mut record = c.clone();
        if !private_file {
            self.credential_entry(c)?.set_password(&c.token)
            .context("credential store unavailable; unlock it or explicitly choose private-file credential storage")?;
            record.token.clear();
        }
        self.write("login", &record)
    }
    pub fn scope(&self, c: &Credentials) -> String {
        // Replacing a hosted server's identity starts a different local history even if its URL is reused.
        let mut scope = format!("{}\n{}", c.url, c.account);
        if let Some(ca) = &c.paired_ca {
            scope.push('\n');
            scope.push_str(&digest(ca.replace('\r', "").as_bytes()));
        }
        digest(scope.as_bytes())
    }
    fn game_key(&self, c: &Credentials, g: &str) -> String {
        format!("game/{}/{}", self.scope(c), g)
    }
    pub fn setup(&self, c: &Credentials, g: &str) -> Result<Setup> {
        self.maybe_setup(c, g)?
            .context("configure this game on this device first")
    }
    pub fn maybe_setup(&self, c: &Credentials, g: &str) -> Result<Option<Setup>> {
        self.read(&self.game_key(c, g))
    }
    pub fn setups(&self, c: &Credentials) -> Result<Vec<Setup>> {
        let db = self.db()?;
        let mut stmt = db.prepare("SELECT v FROM state WHERE k GLOB ? ORDER BY k")?;
        stmt.query_map([self.game_key(c, "*")], |r| r.get::<_, String>(0))?
            .map(|r| Ok(serde_json::from_str(&r?)?))
            .collect()
    }
    pub fn save(&self, c: &Credentials, s: &Setup) -> Result<()> {
        self.write(&self.game_key(c, &s.recipe.game), s)
    }
    pub fn lock(&self, key: &str) -> Result<fs::File> {
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.home.join(format!("{}.lock", digest(key.as_bytes()))))?;
        file.try_lock()
            .context("another Buddy operation holds this game or file selection")?;
        Ok(file)
    }
    pub fn cache_lock(&self, c: &Credentials, exclusive: bool) -> Result<fs::File> {
        let f = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.home.join(format!("{}-cache.lock", self.scope(c))))?;
        if exclusive {
            f.try_lock()
        } else {
            f.try_lock_shared()
        }
        .context("cache is in use")?;
        Ok(f)
    }
    pub fn clean_cache(&self) -> Result<usize> {
        let c = self.credentials()?;
        let _lock = self.cache_lock(&c, true)?;
        let cache = self.cache(&c)?;
        let mut keep = std::collections::BTreeSet::new();
        for game in self.setups(&c)? {
            let mut mark = |tree: &Tree| -> Result<()> {
                keep.extend(tree.chunks()?.into_keys());
                Ok(())
            };
            for tree in game.base.iter().chain(game.active_base.iter()) {
                mark(tree)?;
            }
            for p in game.pending {
                mark(&p.tree)?;
                if let Some(tree) = p.base {
                    mark(&tree)?;
                }
            }
            if let Some(plan) = game.restore {
                mark(&plan.before)?;
                mark(&plan.after)?;
            }
        }
        let mut count = 0;
        for entry in fs::read_dir(cache.0)? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if hash_ok(name) && !keep.contains(name) {
                fs::remove_file(entry.path())?;
                count += 1;
            }
        }
        Ok(count)
    }
    pub fn named_roots(&self, c: &Credentials, game: &str) -> Result<BTreeMap<String, PathBuf>> {
        Ok(self
            .read(&format!("roots/{}/{}", self.scope(c), game))?
            .unwrap_or_default())
    }
    pub fn launcher(&self, c: &Credentials, game: &str) -> Result<Option<Launcher>> {
        self.read(&format!("launcher/{}/{game}", self.scope(c)))
    }
    pub fn save_launcher(&self, game: &str, mut command: Launcher) -> Result<()> {
        ensure!(key(game), "invalid game ID");
        let c = self.credentials()?;
        let _lock = self.lock(&format!("{}:{game}", self.scope(&c)))?;
        command.executable = command.executable.canonicalize()?;
        command.directory = command.directory.canonicalize()?;
        ensure!(
            command.executable.is_file() && command.directory.is_dir(),
            "choose an executable and a working directory"
        );
        self.write(&format!("launcher/{}/{game}", self.scope(&c)), &command)
    }
    pub fn bind_root(&self, game: &str, name: &str, path: &Path) -> Result<()> {
        ensure!(
            key(game)
                && key(name)
                && !matches!(
                    name,
                    "home" | "data" | "config" | "install" | "documents" | "pfx"
                ),
            "choose a custom root name"
        );
        let c = self.credentials()?;
        let _lock = self.lock(&format!("{}:{game}", self.scope(&c)))?;
        let p = path.canonicalize()?;
        ensure!(p.is_dir(), "select a directory");
        let mut roots = self.named_roots(&c, game)?;
        roots.insert(name.into(), p);
        self.write(&format!("roots/{}/{}", self.scope(&c), game), &roots)
    }
    pub fn cache(&self, c: &Credentials) -> Result<Cache> {
        let path = self.home.join("chunks").join(self.scope(c));
        fs::create_dir_all(&path)?;
        Cache::open(path)
    }
}
pub fn canonical_url(url: &str) -> Result<String> {
    let u = reqwest::Url::parse(url.trim())?;
    ensure!(
        u.username().is_empty()
            && u.password().is_none()
            && u.query().is_none()
            && u.fragment().is_none(),
        "enter a server URL without credentials, query or fragment"
    );
    let host = u
        .host_str()
        .context("missing host")?
        .trim_matches(['[', ']']);
    let loopback = host.eq_ignore_ascii_case("localhost")
        || host.parse::<IpAddr>().is_ok_and(|a| a.is_loopback());
    ensure!(
        u.scheme() == "https" || u.scheme() == "http" && loopback,
        "use HTTPS for remote servers"
    );
    Ok(u.as_str().trim_end_matches('/').to_owned())
}
#[derive(Clone)]
pub struct Api {
    pub c: Credentials,
    client: Client,
}
fn http(paired_ca: Option<&str>) -> Result<Client> {
    let mut builder = Client::builder();
    if let Some(ca) = paired_ca {
        builder = builder
            .tls_built_in_root_certs(false)
            .add_root_certificate(reqwest::Certificate::from_pem(ca.as_bytes())?);
    }
    Ok(builder
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(4))
        .timeout(Duration::from_secs(90))
        .build()?)
}
#[derive(Debug)]
pub struct ApiError(pub u16, pub String);
impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "server returned {}: {}", self.0, self.1)
    }
}
impl std::error::Error for ApiError {}
fn checked(r: Response) -> Result<Response> {
    let code = r.status();
    if code.is_success() {
        Ok(r)
    } else {
        let v = r.json::<serde_json::Value>().unwrap_or_default();
        Err(ApiError(
            code.as_u16(),
            v["error"].as_str().unwrap_or("request failed").to_owned(),
        )
        .into())
    }
}
fn ambiguous_reply(e: &anyhow::Error) -> bool {
    if let Some(status) = e.downcast_ref::<ApiError>() {
        return matches!(status.0, 502..=504);
    }
    if let Some(http) = e.downcast_ref::<reqwest::Error>() {
        if http.is_connect() {
            return unreachable(e);
        }
        return http.is_timeout() || http.is_request() || http.is_body() || http.is_decode();
    }
    false
}
pub fn unreachable(e: &anyhow::Error) -> bool {
    let Some(http) = e.downcast_ref::<reqwest::Error>() else {
        return false;
    };
    if http.is_timeout() {
        return true;
    }
    if !http.is_connect() {
        return false;
    }
    use std::io::ErrorKind as K;
    e.chain().any(|cause| {
        cause.downcast_ref::<std::io::Error>().is_some_and(|io| {
            matches!(
                io.kind(),
                K::ConnectionRefused
                    | K::NetworkUnreachable
                    | K::HostUnreachable
                    | K::TimedOut
                    | K::NotConnected
                    | K::AddrNotAvailable
            )
        }) || cause.to_string().starts_with("dns error")
    })
}
impl Api {
    pub fn login_paired(
        url: &str,
        login: Login,
        pair: Option<&crate::host::Pairing>,
    ) -> Result<Credentials> {
        let url = canonical_url(url)?;
        let paired_ca = if let Some(pair) = pair {
            pair.validate()?;
            ensure!(
                pair.url == url,
                "pairing file belongs to a different server"
            );
            Some(pair.ca.clone())
        } else {
            None
        };
        let token = checked(
            http(paired_ca.as_deref())?
                .post(format!("{url}/v1/login"))
                .json(&login)
                .send()?,
        )?
        .json()?;
        Ok(Credentials {
            url,
            account: login.account,
            device: login.device,
            token,
            paired_ca,
        })
    }
    pub fn new(c: Credentials) -> Result<Self> {
        if c.paired_ca.is_some() {
            ensure!(
                canonical_url(&c.url)?.starts_with("https://"),
                "a paired server requires HTTPS"
            );
        }
        let client = http(c.paired_ca.as_deref())?;
        Ok(Self { c, client })
    }
    pub fn rpc<T: DeserializeOwned>(&self, a: Action) -> Result<T> {
        let send = || -> Result<T> {
            let response = self
                .client
                .post(format!("{}/v1/rpc", self.c.url))
                .bearer_auth(&self.c.token)
                .json(&a)
                .send()?;
            Ok(checked(response)?.json()?)
        };
        match send() {
            Err(e) if a.operation_id().is_some() && ambiguous_reply(&e) => send(),
            result => result,
        }
    }
    pub fn upload(
        &self,
        cache: &Cache,
        tree: &Tree,
        mut progress: impl FnMut(u64, u64),
    ) -> Result<()> {
        let sizes = tree.chunks()?;
        let keys: Vec<_> = sizes.keys().cloned().collect();
        let mut needed = vec![];
        for batch in keys.chunks(1024) {
            let missing: Vec<String> = self.rpc(Action::Missing {
                hashes: batch.to_vec(),
            })?;
            for hash in missing {
                let size = *sizes.get(&hash).context("unexpected missing chunk")?;
                needed.push((hash, size));
            }
        }
        let total = needed.iter().map(|(_, s)| *s as u64).sum();
        let mut done = 0;
        progress(done, total);
        for group in crate::pack::groups(&needed) {
            let mut body = vec![];
            for (h, _) in group {
                crate::pack::append(&mut body, h, &cache.get(h)?)?;
            }
            checked(
                self.client
                    .post(format!("{}/v1/packs", self.c.url))
                    .bearer_auth(&self.c.token)
                    .body(body)
                    .send()?,
            )?;
            done += group.iter().map(|(_, s)| *s as u64).sum::<u64>();
            progress(done, total);
        }
        Ok(())
    }
    pub fn download(
        &self,
        cache: &Cache,
        tree: &Tree,
        mut progress: impl FnMut(u64, u64),
    ) -> Result<()> {
        use std::io::Read;
        let needed: Vec<_> = tree
            .chunks()?
            .into_iter()
            .filter(|(h, _)| cache.get(h).is_err())
            .collect();
        let total = needed.iter().map(|(_, s)| *s as u64).sum();
        let mut done = 0;
        progress(done, total);
        for group in crate::pack::groups(&needed) {
            let hashes: Vec<_> = group.iter().map(|(h, _)| h).collect();
            let r = checked(
                self.client
                    .post(format!("{}/v1/packs/read", self.c.url))
                    .bearer_auth(&self.c.token)
                    .json(&hashes)
                    .send()?,
            )?;
            let mut bytes = vec![];
            r.take(crate::pack::LIMIT as u64 + 1)
                .read_to_end(&mut bytes)?;
            let frames = crate::pack::decode(&bytes)?;
            ensure!(frames.len() == group.len(), "incomplete pack");
            for ((hash, data), (expected, size)) in frames.into_iter().zip(group) {
                ensure!(
                    hash == expected && data.len() == *size as usize,
                    "unexpected packed chunk"
                );
                cache.put(hash, data)?;
                done += data.len() as u64;
            }
            progress(done, total);
        }
        Ok(())
    }
}

pub fn default_device() -> String {
    hostname::get()
        .ok()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "PC".into())
}
fn steam_installs() -> Vec<steamlocate::SteamDir> {
    match std::env::var_os("BUDDY_STEAM_ROOT") {
        Some(p) => steamlocate::SteamDir::from_dir(Path::new(&p)).map(|s| vec![s]),
        None => steamlocate::locate_all(),
    }
    .unwrap_or_default()
}

pub fn has_cloud_history(game: &str) -> bool {
    let Some(id) = game
        .strip_prefix("steam-")
        .and_then(|s| s.parse::<u32>().ok())
    else {
        return false;
    };
    let app = id.to_string();
    steam_installs().iter().any(|steam| {
        steam.path().join("userdata").read_dir().is_ok_and(|users| {
            users
                .flatten()
                .any(|user| user.path().join(&app).join("remotecache.vdf").is_file())
        })
    })
}
pub fn discover() -> Vec<(String, String, PathBuf)> {
    let mut games = BTreeMap::new();
    for install in steam_installs() {
        if let Ok(libs) = install.libraries() {
            for lib in libs.flatten() {
                for app in lib.apps().flatten() {
                    let name = app.name.clone().unwrap_or_else(|| app.install_dir.clone());
                    games.insert(
                        format!("steam-{}", app.app_id),
                        (name, lib.resolve_app_dir(&app)),
                    );
                }
            }
        }
        if let Ok(shortcuts) = install.shortcuts() {
            for shortcut in shortcuts.flatten() {
                let exe = PathBuf::from(shortcut.executable.trim_matches('"'));
                let folder = exe
                    .parent()
                    .filter(|p| p.is_dir())
                    .unwrap_or(Path::new(&shortcut.start_dir))
                    .to_owned();
                games.insert(
                    format!("shortcut-{}", shortcut.app_id),
                    (shortcut.app_name, folder),
                );
            }
        }
    }
    games
        .into_iter()
        .map(|(id, (name, p))| (id, name, p))
        .collect()
}
pub fn roots(game: &str) -> BTreeMap<String, PathBuf> {
    let mut roots = BTreeMap::new();
    if let Some(b) = directories::BaseDirs::new() {
        roots.insert("home".into(), b.home_dir().to_owned());
        roots.insert("data".into(), b.data_dir().to_owned());
        roots.insert("config".into(), b.config_dir().to_owned());
    }
    if let Some(p) = directories::UserDirs::new().and_then(|b| b.document_dir().map(Path::to_owned))
    {
        roots.insert("documents".into(), p);
    }
    if let Some((_, _, install)) = discover().into_iter().find(|(id, ..)| id == game) {
        if let Some(apps) = install.parent().and_then(Path::parent)
            && let Some(id) = game.strip_prefix("steam-")
        {
            let pfx = apps.join("compatdata").join(id).join("pfx");
            if pfx.is_dir() {
                roots.insert("pfx".into(), pfx);
            }
        }
        roots.insert("install".into(), install);
    }
    if let Some(p) = std::env::var_os("STEAM_COMPAT_DATA_PATH").filter(|_| {
        std::env::var("SteamAppId")
            .ok()
            .is_some_and(|id| game == format!("steam-{id}"))
    }) {
        roots.insert("pfx".into(), PathBuf::from(p).join("pfx"));
    }
    roots
}
pub fn location(path: &Path, roots: &BTreeMap<String, PathBuf>) -> Option<Location> {
    let (name, root) = roots
        .iter()
        .flat_map(|(name, p)| {
            std::iter::once(p.clone())
                .chain(p.canonicalize().ok())
                .map(move |root| (name, root))
        })
        .filter(|(_, p)| path.starts_with(p))
        .max_by_key(|(_, p)| p.components().count())?;
    Some(Location {
        root: name.clone(),
        relative: path
            .strip_prefix(root)
            .ok()?
            .components()
            .map(|c| c.as_os_str().to_str())
            .collect::<Option<Vec<_>>>()?
            .join("/"),
    })
}

impl Local {
    pub fn lock_paths<'a>(&self, paths: impl Iterator<Item = &'a Path>) -> Result<Vec<fs::File>> {
        let mut modes = BTreeMap::<PathBuf, bool>::new();
        for path in paths {
            for p in path.ancestors() {
                let mode = modes.entry(p.to_owned()).or_default();
                *mode |= p == path;
            }
        }
        let mut locks = vec![];
        for (p, exclusive) in modes {
            let name = digest(p.to_string_lossy().as_bytes());
            let f = fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(self.home.join(format!("path-{name}.lock")))?;
            if exclusive {
                f.try_lock()
            } else {
                f.try_lock_shared()
            }
            .with_context(|| format!("a game is already using {}", p.display()))?;
            locks.push(f);
        }
        Ok(locks)
    }
}
