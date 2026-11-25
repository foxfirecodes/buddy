use crate::model::*;
use anyhow::{Context, Result, anyhow, ensure};
use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier, password_hash::SaltString};
use axum::{
    Json, Router,
    body::Bytes,
    extract::{DefaultBodyLimit, Path as UrlPath, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    fs,
    io::Write,
    net::SocketAddr,
    path::{Path, PathBuf},
    time::Duration,
};

mod releases;
pub use releases::{publish_release, status};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    listen: SocketAddr,
    database: PathBuf,
    account_quota_bytes: Option<u64>,
    tls: Option<TlsFiles>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TlsFiles {
    certificate: PathBuf,
    private_key: PathBuf,
}
#[derive(Clone)]
pub struct Store(PathBuf, Option<u64>);
type ApiResult<T> = std::result::Result<T, ApiError>;
struct ApiError(StatusCode, String);
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"error":self.1}))).into_response()
    }
}
#[derive(Debug)]
struct Refused(StatusCode, String);
impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.1.fmt(f)
    }
}
impl std::error::Error for Refused {}
fn denied(code: StatusCode, msg: &str) -> anyhow::Error {
    Refused(code, msg.to_owned()).into()
}
fn stale() -> anyhow::Error {
    denied(
        StatusCode::PRECONDITION_FAILED,
        "session or head changed; reconcile again",
    )
}
async fn database<T: Send + 'static>(
    f: impl FnOnce() -> Result<T> + Send + 'static,
) -> ApiResult<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|_| {
            ApiError(
                StatusCode::INTERNAL_SERVER_ERROR,
                "database task failed".into(),
            )
        })?
        .map_err(|e| {
            if let Some(r) = e.downcast_ref::<Refused>() {
                ApiError(r.0, r.1.clone())
            } else if e.is::<rusqlite::Error>() || e.is::<std::io::Error>() {
                eprintln!("Buddy server: {e:#}");
                ApiError(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "storage operation failed".into(),
                )
            } else {
                ApiError(StatusCode::BAD_REQUEST, e.to_string())
            }
        })
}
fn bearer(h: &HeaderMap) -> String {
    h.get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .unwrap_or("")
        .to_owned()
}
struct User {
    account: String,
    device: String,
    name: String,
}
fn who(c: &Connection, token: &str) -> Result<User> {
    c.query_row(
        "SELECT account,id,name FROM devices WHERE token=?",
        [digest(token.as_bytes())],
        |r| {
            Ok(User {
                account: r.get(0)?,
                device: r.get(1)?,
                name: r.get(2)?,
            })
        },
    )
    .optional()?
    .ok_or_else(|| denied(StatusCode::UNAUTHORIZED, "sign in again"))
}
impl Store {
    fn connection(&self) -> Result<Connection> {
        let c = Connection::open(&self.0)?;
        c.busy_timeout(Duration::from_secs(10))?;
        c.execute_batch("PRAGMA foreign_keys=ON; PRAGMA synchronous=FULL;")?;
        Ok(c)
    }
    fn from_config(path: &Path) -> Result<(Self, Config)> {
        let settings = fs::read_to_string(path).with_context(|| {
            format!(
                "open {} (use --config PATH or run buddy server init for a new server)",
                path.display()
            )
        })?;
        let cfg: Config = toml::from_str(&settings).context("parse server settings")?;
        let db = path.parent().unwrap_or(Path::new(".")).join(&cfg.database);
        Ok((Self(db, cfg.account_quota_bytes), cfg))
    }
    fn open(path: &Path) -> Result<(Self, Config)> {
        let (store, cfg) = Self::from_config(path)?;
        if let Some(parent) = store.0.parent() {
            fs::create_dir_all(parent)?;
        }
        let c = store.connection()?;
        c.execute_batch("PRAGMA journal_mode=WAL;
          CREATE TABLE IF NOT EXISTS accounts(name TEXT PRIMARY KEY,password TEXT NOT NULL);
          CREATE TABLE IF NOT EXISTS invites(hash TEXT PRIMARY KEY,expires INTEGER NOT NULL);
          CREATE TABLE IF NOT EXISTS device_links(hash TEXT PRIMARY KEY,account TEXT NOT NULL,expires INTEGER NOT NULL);
          CREATE TABLE IF NOT EXISTS releases(platform TEXT NOT NULL,generation INTEGER NOT NULL,sha256 TEXT NOT NULL,installer_hash TEXT NOT NULL,installer TEXT NOT NULL,manifest TEXT NOT NULL,PRIMARY KEY(platform,generation));
          CREATE INDEX IF NOT EXISTS published_binary ON releases(sha256);
          CREATE TABLE IF NOT EXISTS devices(id TEXT PRIMARY KEY,account TEXT NOT NULL,name TEXT NOT NULL,token TEXT NOT NULL UNIQUE);
          CREATE TABLE IF NOT EXISTS attempts(peer TEXT PRIMARY KEY, starts INTEGER NOT NULL,n INTEGER NOT NULL);
          CREATE TABLE IF NOT EXISTS games(account TEXT,game TEXT,recipe TEXT NOT NULL,revision INTEGER NOT NULL,shared INTEGER NOT NULL,head INTEGER,
            generation INTEGER NOT NULL DEFAULT 0,holder TEXT,session TEXT,since INTEGER,expires INTEGER,PRIMARY KEY(account,game));
          CREATE TABLE IF NOT EXISTS retired_slots(account TEXT,game TEXT,name TEXT,kind TEXT NOT NULL,PRIMARY KEY(account,game,name));
          CREATE TABLE IF NOT EXISTS objects(account TEXT,hash TEXT,bytes BLOB NOT NULL,touched INTEGER NOT NULL,PRIMARY KEY(account,hash));
          CREATE TABLE IF NOT EXISTS versions(id INTEGER PRIMARY KEY AUTOINCREMENT,account TEXT NOT NULL,game TEXT NOT NULL,origin TEXT NOT NULL,
            created INTEGER NOT NULL,tree TEXT NOT NULL,pin TEXT NOT NULL DEFAULT '',published INTEGER NOT NULL);
          CREATE TABLE IF NOT EXISTS backup_changes(version INTEGER PRIMARY KEY REFERENCES versions(id) ON DELETE CASCADE,changes TEXT NOT NULL);
          CREATE INDEX IF NOT EXISTS history ON versions(account,game,id DESC);
          CREATE TABLE IF NOT EXISTS refs(version INTEGER REFERENCES versions(id) ON DELETE CASCADE,hash TEXT NOT NULL,PRIMARY KEY(version,hash));
          CREATE TABLE IF NOT EXISTS candidates(account TEXT,game TEXT,device TEXT,head INTEGER,hash TEXT,version INTEGER REFERENCES versions(id) ON DELETE CASCADE,
            PRIMARY KEY(account,game,device,head,hash));
          CREATE TABLE IF NOT EXISTS operations(account TEXT,op TEXT,request TEXT NOT NULL,result INTEGER NOT NULL,PRIMARY KEY(account,op));")?;
        Ok((store, cfg))
    }
}
pub fn invite(path: &Path) -> Result<String> {
    let (s, _) = Store::open(path)?;
    let code = id();
    s.connection()?.execute(
        "INSERT INTO invites VALUES(?,?)",
        params![digest(code.as_bytes()), now() + 86400],
    )?;
    Ok(code)
}
fn sweep(s: &Store) -> Result<usize> {
    Ok(s.connection()?.execute("DELETE FROM objects WHERE touched<? AND NOT EXISTS (SELECT 1 FROM refs JOIN versions v ON v.id=refs.version WHERE v.account=objects.account AND refs.hash=objects.hash)",[now()-86400])?)
}
pub fn gc(path: &Path) -> Result<usize> {
    sweep(&Store::open(path)?.0)
}
fn check_quota(tx: &Transaction, s: &Store, account: &str) -> Result<()> {
    if let Some(limit) = s.1 {
        let used: u64 = tx.query_row(
            "SELECT coalesce(sum(length(bytes)),0) FROM objects WHERE account=?",
            [account],
            |r| r.get(0),
        )?;
        if used > limit {
            return Err(denied(
                StatusCode::INSUFFICIENT_STORAGE,
                "account storage quota exceeded; pending saves remain on the device",
            ));
        }
    }
    Ok(())
}
pub fn init_config() -> Result<()> {
    let mut config = tempfile::NamedTempFile::new_in(".").context("create server.toml")?;
    config.write_all(include_bytes!("../../server.example.toml"))?;
    config.as_file().sync_all()?;
    if let Err(err) = config.persist_noclobber("server.toml") {
        if err.error.kind() == std::io::ErrorKind::AlreadyExists {
            anyhow::bail!("server.toml already exists");
        }
        return Err(err.error).context("create server.toml");
    }
    Ok(())
}
pub fn serve(path: &Path) -> Result<()> {
    serve_controlled(path, axum_server::Handle::new(), |addr| {
        eprintln!("Buddy listening on {addr}")
    })
}
pub fn serve_controlled(
    path: &Path,
    handle: axum_server::Handle<SocketAddr>,
    ready: impl FnOnce(SocketAddr),
) -> Result<()> {
    let (s, cfg) = Store::open(path)?;
    tokio::runtime::Runtime::new()?.block_on(async move {
        let maintenance = s.clone();
        tokio::spawn(async move {
            loop {
                let store = maintenance.clone();
                if let Err(e) = tokio::task::spawn_blocking(move || sweep(&store))
                    .await
                    .unwrap_or_else(|e| Err(anyhow!(e)))
                {
                    eprintln!("Buddy storage cleanup: {e}");
                }
                tokio::time::sleep(Duration::from_secs(6 * 3600)).await;
            }
        });
        let app = releases::routes().merge(Router::new()
            .route("/v1/login", post(login))
            .route("/v1/rpc", post(rpc))
            .route(
                "/v1/packs",
                post(put_pack).layer(DefaultBodyLimit::max(crate::pack::LIMIT)),
            )
            .route("/v1/packs/read", post(get_pack))
            .route(
                "/v1/chunks/{hash}",
                get(download)
                    .put(upload)
                    .layer(DefaultBodyLimit::max(MAX_CHUNK)),
            )
            .layer(DefaultBodyLimit::max(32 * 1024 * 1024)))
            .with_state(s);
        let listener = std::net::TcpListener::bind(cfg.listen)?;
        listener.set_nonblocking(true)?;
        let addr = listener.local_addr()?;
        let service = app.into_make_service_with_connect_info::<SocketAddr>();
        if let Some(tls) = cfg.tls {
            let dir = path.parent().unwrap_or(Path::new("."));
            let tls = axum_server::tls_rustls::RustlsConfig::from_pem_file(
                dir.join(tls.certificate),
                dir.join(tls.private_key),
            )
            .await?;
            let server = axum_server::from_tcp_rustls(listener, tls)?.handle(handle);
            ready(addr);
            server.serve(service).await?;
        } else {
            ready(addr);
            if !addr.ip().to_canonical().is_loopback() {
                eprintln!("Warning: serving HTTP on {addr}. Buddy sends passwords, device tokens, and save data without TLS.");
            }
            axum::serve(tokio::net::TcpListener::from_std(listener)?, service).await?;
        }
        Ok(())
    })
}
async fn login(
    State(s): State<Store>,
    axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<SocketAddr>,
    Json(l): Json<Login>,
) -> ApiResult<Json<String>> {
    database(move || sign_in(&s, peer, l).map(Json)).await
}

fn sign_in(store: &Store, peer: SocketAddr, login: Login) -> Result<String> {
    ensure!(
        key(&login.account)
            && !login.device.trim().is_empty()
            && login.device.len() <= 120
            && login.password.len() <= 1024,
        "invalid sign-in fields"
    );
    let mut connection = store.connection()?;
    let attempts: i64 = connection.query_row(
        "INSERT INTO attempts VALUES(?1,?2,1)
         ON CONFLICT(peer) DO UPDATE SET
           n=CASE WHEN starts < ?2-60 THEN 1 ELSE n+1 END,
           starts=CASE WHEN starts < ?2-60 THEN ?2 ELSE starts END RETURNING n",
        params![peer.ip().to_string(), now()],
        |r| r.get(0),
    )?;
    if attempts > 60 {
        return Err(denied(
            StatusCode::TOO_MANY_REQUESTS,
            "too many sign-in attempts; retry in a minute",
        ));
    }
    // Hash outside the write transaction so game heartbeats can run during registration.
    let registration = if let Some(invite) = &login.invite {
        ensure!(login.password.len() >= 8, "use at least eight characters");
        let salt = SaltString::encode_b64(uuid::Uuid::new_v4().as_bytes())
            .map_err(|e| anyhow!(e.to_string()))?;
        let hash = Argon2::default()
            .hash_password(login.password.as_bytes(), &salt)
            .map_err(|e| anyhow!(e.to_string()))?
            .to_string();
        Some((digest(invite.as_bytes()), hash))
    } else {
        let hash: Option<String> = connection
            .query_row(
                "SELECT password FROM accounts WHERE name=?",
                [&login.account],
                |r| r.get(0),
            )
            .optional()?;
        let verified = hash
            .as_ref()
            .and_then(|h| PasswordHash::new(h).ok())
            .is_some_and(|h| {
                Argon2::default()
                    .verify_password(login.password.as_bytes(), &h)
                    .is_ok()
            });
        if !verified {
            return Err(denied(StatusCode::UNAUTHORIZED, "invalid sign-in"));
        }
        None
    };
    let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if let Some((invite, hash)) = registration {
        let used = tx.execute(
            "DELETE FROM invites WHERE hash=? AND expires>?",
            params![invite, now()],
        )?;
        ensure!(used == 1, "invalid or expired invite");
        let created = tx.execute(
            "INSERT OR IGNORE INTO accounts VALUES(?,?)",
            params![login.account, hash],
        )?;
        ensure!(created == 1, "account already exists");
    }
    let token = format!("{}{}", id(), id());
    tx.execute(
        "INSERT INTO devices VALUES(?,?,?,?)",
        params![id(), login.account, login.device, digest(token.as_bytes())],
    )?;
    tx.commit()?;
    Ok(token)
}

const PUT_CHUNK: &str = "INSERT INTO objects VALUES(?,?,?,?)
    ON CONFLICT(account,hash) DO UPDATE SET touched=excluded.touched";

async fn upload(
    State(store): State<Store>,
    UrlPath(hash): UrlPath<String>,
    headers: HeaderMap,
    bytes: Bytes,
) -> ApiResult<StatusCode> {
    database(move || {
        let mut connection = store.connection()?;
        let user = who(&connection, &bearer(&headers))?;
        ensure!(
            !bytes.is_empty() && bytes.len() <= MAX_CHUNK && digest(&bytes) == hash,
            "invalid chunk"
        );
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            PUT_CHUNK,
            params![user.account, hash, bytes.as_ref(), now()],
        )?;
        check_quota(&tx, &store, &user.account)?;
        tx.commit()?;
        Ok(StatusCode::NO_CONTENT)
    })
    .await
}
async fn download(
    State(s): State<Store>,
    UrlPath(hash): UrlPath<String>,
    h: HeaderMap,
) -> ApiResult<Vec<u8>> {
    database(move || {
        let c = s.connection()?;
        let u = who(&c, &bearer(&h))?;
        c.query_row(
            "SELECT bytes FROM objects WHERE account=? AND hash=?",
            params![u.account, hash],
            |r| r.get(0),
        )
        .optional()?
        .ok_or_else(|| denied(StatusCode::NOT_FOUND, "chunk unavailable"))
    })
    .await
}
async fn put_pack(State(s): State<Store>, h: HeaderMap, bytes: Bytes) -> ApiResult<StatusCode> {
    database(move || {
        let mut connection = s.connection()?;
        let user = who(&connection, &bearer(&h))?;
        let entries = crate::pack::decode(&bytes)?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        {
            let mut insert = tx.prepare_cached(PUT_CHUNK)?;
            for (hash, data) in entries {
                insert.execute(params![user.account, hash, data, now()])?;
            }
        }
        check_quota(&tx, &s, &user.account)?;
        tx.commit()?;
        Ok(StatusCode::NO_CONTENT)
    })
    .await
}
async fn get_pack(
    State(s): State<Store>,
    h: HeaderMap,
    Json(hashes): Json<Vec<String>>,
) -> ApiResult<Vec<u8>> {
    database(move || {
        let c = s.connection()?;
        let u = who(&c, &bearer(&h))?;
        ensure!(hashes.len() <= crate::pack::COUNT, "pack too large");
        let mut out = vec![];
        let mut query = c.prepare_cached("SELECT bytes FROM objects WHERE account=? AND hash=?")?;
        for hash in hashes {
            let bytes: Vec<u8> = query
                .query_row(params![u.account, hash], |r| r.get(0))
                .optional()?
                .context("chunk unavailable")?;
            crate::pack::append(&mut out, &hash, &bytes)?;
        }
        Ok(out)
    })
    .await
}
async fn rpc(
    State(s): State<Store>,
    h: HeaderMap,
    Json(a): Json<Action>,
) -> ApiResult<Json<Value>> {
    database(move || {
        let mut c = s.connection()?;
        let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let u = who(&tx, &bearer(&h))?;
        let result = apply(&tx, &u, a)?;
        tx.commit()?;
        Ok(Json(result))
    })
    .await
}
struct Game {
    recipe: Recipe,
    revision: i64,
    shared: bool,
    head: Option<i64>,
    generation: i64,
    holder: Option<String>,
    session: Option<String>,
    since: i64,
    expires: i64,
}
fn game(c: &Connection, u: &User, g: &str) -> Result<Game> {
    let sql = "SELECT recipe,head,generation,holder,session,coalesce(since,0),coalesce(expires,0),revision,shared
        FROM games WHERE account=? AND game=?";
    c.query_row(sql, params![u.account, g], |row| {
        let raw: String = row.get(0)?;
        // Store the decoded recipe alongside the lease snapshot from this one query.
        let recipe = serde_json::from_str(&raw).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
        })?;
        Ok(Game {
            recipe,
            head: row.get(1)?,
            generation: row.get(2)?,
            holder: row.get(3)?,
            session: row.get(4)?,
            since: row.get(5)?,
            expires: row.get(6)?,
            revision: row.get(7)?,
            shared: row.get(8)?,
        })
    })
    .optional()?
    .context("game has no recipe on this account")
}
fn owned(c: &Connection, u: &User, l: &Lease) -> Result<Game> {
    let g = game(c, u, &l.game)?;
    if g.generation != l.generation
        || g.holder.as_deref() != Some(&u.device)
        || g.session.as_ref() != Some(&l.session)
        || g.expires <= now()
    {
        return Err(stale());
    }
    Ok(g)
}
fn release(c: &Connection, u: &User, g: &str) -> Result<()> {
    c.execute(
        "UPDATE games SET holder=NULL,session=NULL,expires=NULL WHERE account=? AND game=?",
        params![u.account, g],
    )?;
    Ok(())
}
fn version(c: &Connection, u: &User, g: &str, id: i64) -> Result<Revision> {
    let (origin, time, tree) = c
        .query_row(
            "SELECT origin,created,tree FROM versions WHERE account=? AND game=? AND id=?",
            params![u.account, g, id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get::<_, String>(2)?)),
        )
        .optional()?
        .context("backup is unavailable")?;
    Ok(Revision {
        id,
        origin,
        time,
        tree: serde_json::from_str(&tree)?,
    })
}
fn backup_changes(tx: &Transaction, u: &User, game: &str, id: i64) -> Result<BackupChanges> {
    let stored: Option<String> = tx.query_row(
        "SELECT c.changes FROM backup_changes c JOIN versions v ON v.id=c.version WHERE v.account=? AND v.game=? AND v.id=?",
        params![u.account, game, id], |r| r.get(0)
    ).optional()?;
    if let Some(raw) = stored {
        return Ok(serde_json::from_str(&raw)?);
    }
    let current = version(tx, u, game, id)?;
    // Old databases did not record parentage. Retention can leave gaps in this comparison.
    let previous: Option<i64> = tx.query_row(
        "SELECT max(id) FROM versions WHERE account=? AND game=? AND id<? AND published",
        params![u.account, game, id],
        |r| r.get(0),
    )?;
    let base = previous.map(|id| version(tx, u, game, id)).transpose()?;
    Ok(BackupChanges::between(
        &base.map(|r| r.tree).unwrap_or_default(),
        &current.tree,
        previous.map_or(Comparison::Contents, Comparison::EarlierRetained),
    ))
}
fn snapshot(
    tx: &Transaction,
    u: &User,
    g: &Game,
    tree: &Tree,
    time: i64,
    pin: &str,
    published: bool,
) -> Result<i64> {
    tree.validate(&g.recipe)?;
    tx.execute("INSERT INTO versions(account,game,origin,created,tree,pin,published) VALUES(?,?,?,?,?,?,?)",params![u.account,g.recipe.game,u.name,time,serde_json::to_string(tree)?,pin,published])?;
    let id = tx.last_insert_rowid();
    let base = g
        .head
        .map(|parent| version(tx, u, &g.recipe.game, parent))
        .transpose()?;
    let changes = BackupChanges::between(
        &base.map(|r| r.tree).unwrap_or_default(),
        tree,
        g.head.map_or(Comparison::First, Comparison::Previous),
    );
    tx.execute(
        "INSERT INTO backup_changes(version,changes) VALUES(?,?)",
        params![id, serde_json::to_string(&changes)?],
    )?;
    let mut lookup =
        tx.prepare_cached("SELECT length(bytes) FROM objects WHERE account=? AND hash=?")?;
    let mut add = tx.prepare_cached("INSERT INTO refs VALUES(?,?)")?;
    for (hash, size) in tree.chunks()? {
        let length: Option<u32> = lookup
            .query_row(params![u.account, hash], |r| r.get(0))
            .optional()?;
        ensure!(length == Some(size), "snapshot is missing chunk {hash}");
        add.execute(params![id, hash])?;
    }
    Ok(id)
}
fn preserve(tx: &Transaction, u: &User, g: &Game, local: &Tree, time: i64) -> Result<i64> {
    let hash = digest(&serde_json::to_vec(local)?);
    let head = g.head.unwrap_or(0);
    let previous:Option<i64> = tx.query_row(
        "SELECT version FROM candidates WHERE account=? AND game=? AND device=? AND head=? AND hash=?",
        params![u.account,g.recipe.game,u.device,head,hash],|r|r.get(0)).optional()?;
    if let Some(id) = previous {
        tx.execute(
            "UPDATE versions SET pin='conflict' WHERE id=? AND pin=''",
            [head],
        )?;
        return Ok(id);
    }
    let id = snapshot(tx, u, g, local, time, "conflict", false)?;
    if let Some(head) = g.head {
        tx.execute("UPDATE versions SET pin='conflict' WHERE id=?", [head])?;
    }
    tx.execute(
        "INSERT INTO candidates VALUES(?,?,?,?,?,?)",
        params![u.account, g.recipe.game, u.device, head, hash, id],
    )?;
    Ok(id)
}
fn publish_head(tx: &Transaction, u: &User, g: &Game, id: i64) -> Result<()> {
    tx.execute(
        "UPDATE games SET head=? WHERE account=? AND game=?",
        params![id, u.account, g.recipe.game],
    )?;
    tx.execute("DELETE FROM versions WHERE account=?1 AND game=?2 AND published AND pin='' AND id NOT IN (SELECT id FROM versions WHERE account=?1 AND game=?2 AND published AND pin='' AND id<>?3 ORDER BY id DESC LIMIT ?4) AND id<>?3",params![u.account,g.recipe.game,id,g.recipe.retention])?;
    Ok(())
}
fn retired_slots(
    tx: &Transaction,
    u: &User,
    game: &str,
) -> Result<std::collections::BTreeMap<String, Kind>> {
    let mut stmt =
        tx.prepare_cached("SELECT name,kind FROM retired_slots WHERE account=? AND game=?")?;
    let mut slots = std::collections::BTreeMap::new();
    for row in stmt.query_map(params![u.account, game], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })? {
        let (name, kind) = row?;
        slots.insert(name, serde_json::from_str(&kind)?);
    }
    Ok(slots)
}
fn remember(tx: &Transaction, u: &User, op: &str, request: &str, result: i64) -> Result<Value> {
    ensure!(key(op), "invalid operation id");
    tx.execute(
        "INSERT INTO operations VALUES(?,?,?,?)",
        params![u.account, op, request, result],
    )?;
    Ok(json!(result))
}
fn apply(tx: &Transaction, u: &User, a: Action) -> Result<Value> {
    let mut fingerprint = String::new();
    if let Some(op) = a.operation_id() {
        fingerprint = digest(&serde_json::to_vec(&a)?);
        if let Some((prior, result)) = tx
            .query_row(
                "SELECT request,result FROM operations WHERE account=? AND op=?",
                params![u.account, op],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
            )
            .optional()?
        {
            ensure!(prior == fingerprint, "operation id reused");
            return Ok(json!(result));
        }
    }
    match a {
        Action::Games | Action::Catalog => {
            let catalog = matches!(a, Action::Catalog);
            let mut stmt=tx.prepare("SELECT recipe,revision,shared FROM games WHERE (account=? AND ?=0) OR (shared AND ?=1) ORDER BY game,account")?;
            let rows = stmt.query_map(params![u.account, catalog, catalog], |r| {
                Ok((r.get::<_, String>(0)?, r.get(1)?, r.get(2)?))
            })?;
            let mut list = vec![];
            for row in rows {
                let (s, revision, shared) = row?;
                let recipe: Recipe = serde_json::from_str(&s)?;
                list.push(GameInfo {
                    retired_slots: if catalog {
                        Default::default()
                    } else {
                        retired_slots(tx, u, &recipe.game)?.into_keys().collect()
                    },
                    recipe,
                    revision,
                    shared,
                });
            }
            Ok(json!(list))
        }
        Action::SaveRecipe {
            recipe,
            expected,
            shared,
        } => {
            recipe.validate()?;
            let encoded = serde_json::to_string(&recipe)?;
            let n = if expected == 0 {
                ensure!(
                    !recipe.slots.is_empty(),
                    "choose a location to enroll this game"
                );
                tx.execute("INSERT OR IGNORE INTO games(account,game,recipe,revision,shared) VALUES(?,?,?,1,?)",params![u.account,recipe.game,encoded,shared])?
            } else {
                let prior = game(tx, u, &recipe.game)?;
                if prior.revision != expected || prior.holder.is_some() {
                    return Err(stale());
                }
                for name in retired_slots(tx, u, &recipe.game)?.keys() {
                    ensure!(
                        !recipe.slots.contains_key(name),
                        "slot {name} was removed from this recipe; choose a new slot ID"
                    );
                }
                // Old snapshots keep their logical IDs; new bindings require explicit device approval.
                for (name, slot) in prior.recipe.slots {
                    ensure!(
                        recipe.slots.get(&name).is_some_and(|s| s.kind == slot.kind),
                        "removing or changing existing slot types requires a new recipe"
                    );
                }
                tx.execute("UPDATE games SET recipe=?,shared=?,revision=revision+1 WHERE account=? AND game=? AND revision=? AND holder IS NULL",params![encoded,shared,u.account,recipe.game,expected])?
            };
            if n != 1 {
                return Err(stale());
            }
            Ok(json!(expected + 1))
        }
        Action::RemoveSlot {
            game: name,
            slot,
            expected,
        } => {
            let mut g = game(tx, u, &name)?;
            if g.revision != expected || g.holder.is_some() {
                return Err(denied(
                    StatusCode::PRECONDITION_FAILED,
                    "recipe changed or a device is playing; refresh and try again",
                ));
            }
            let removed = g
                .recipe
                .slots
                .remove(&slot)
                .context("slot is absent from this recipe")?;
            tx.execute(
                "INSERT INTO retired_slots VALUES(?,?,?,?)",
                params![u.account, name, slot, serde_json::to_string(&removed.kind)?],
            )?;
            tx.execute(
                "UPDATE games SET recipe=?,revision=revision+1 WHERE account=? AND game=?",
                params![serde_json::to_string(&g.recipe)?, u.account, name],
            )?;
            if let Some(id) = g.head {
                let mut live = version(tx, u, &name, id)?.tree;
                let before = live.0.len();
                live.0
                    .retain(|path, _| path.split('/').next() != Some(slot.as_str()));
                if live.0.len() != before {
                    let id = snapshot(tx, u, &g, &live, now(), "", true)?;
                    publish_head(tx, u, &g, id)?;
                }
            }
            Ok(json!(GameInfo {
                recipe: g.recipe,
                revision: expected + 1,
                shared: g.shared,
                retired_slots: retired_slots(tx, u, &name)?.into_keys().collect()
            }))
        }
        Action::Acquire {
            game: g,
            session,
            takeover,
        } => {
            ensure!(key(&session), "invalid session");
            let state = game(tx, u, &g)?;
            let same = state.session.as_ref() == Some(&session)
                && state.holder.as_ref() == Some(&u.device)
                && state.expires > now();
            if !same && state.holder.is_some() && takeover != Some(state.generation) {
                let device = tx
                    .query_row(
                        "SELECT name FROM devices WHERE id=?",
                        [&state.holder],
                        |r| r.get(0),
                    )
                    .optional()?
                    .unwrap_or_else(|| "revoked device".into());
                return Ok(json!(Acquisition::Busy(Held {
                    generation: state.generation,
                    device,
                    since: state.since,
                    expires: state.expires
                })));
            }
            if takeover.is_some() && takeover != Some(state.generation) {
                return Err(stale());
            }
            let generation = state.generation + i64::from(!same);
            tx.execute("UPDATE games SET generation=?,holder=?,session=?,since=?,expires=? WHERE account=? AND game=?",params![generation,u.device,session,if same{state.since}else{now()},now()+120,u.account,g])?;
            let head = state.head.map(|i| version(tx, u, &g, i)).transpose()?;
            let retired_slots = retired_slots(tx, u, &g)?.into_keys().collect();
            Ok(json!(Acquisition::Granted {
                lease: Lease {
                    game: g,
                    generation,
                    session
                },
                head,
                recipe: state.recipe,
                revision: state.revision,
                shared: state.shared,
                retired_slots,
            }))
        }
        Action::Renew { lease } => {
            owned(tx, u, &lease)?;
            tx.execute(
                "UPDATE games SET expires=? WHERE account=? AND game=?",
                params![now() + 120, u.account, lease.game],
            )?;
            Ok(Value::Null)
        }
        Action::Release { lease } => {
            owned(tx, u, &lease)?;
            release(tx, u, &lease.game)?;
            Ok(Value::Null)
        }
        Action::Missing { hashes } => {
            ensure!(hashes.len() <= 4096, "too many hashes");
            let mut missing = vec![];
            let mut q =
                tx.prepare_cached("UPDATE objects SET touched=? WHERE account=? AND hash=?")?;
            for h in hashes {
                ensure!(hash_ok(&h), "invalid hash");
                if q.execute(params![now(), u.account, h])? == 0 {
                    missing.push(h);
                }
            }
            Ok(json!(missing))
        }
        Action::Commit {
            lease,
            expected,
            tree,
            close,
            op,
        } => {
            let g = owned(tx, u, &lease)?;
            if g.head != expected {
                return Err(stale());
            }
            let old = g.head.map(|i| version(tx, u, &lease.game, i)).transpose()?;
            let id = match old {
                Some(r) if r.tree == tree => r.id,
                _ => snapshot(tx, u, &g, &tree, now(), "", true)?,
            };
            publish_head(tx, u, &g, id)?;
            if close {
                release(tx, u, &lease.game)?;
            }
            remember(tx, u, &op, &fingerprint, id)
        }
        Action::Conflict {
            lease,
            expected,
            local,
            time,
            op,
        } => {
            let g = owned(tx, u, &lease)?;
            if g.head != Some(expected) {
                return Err(stale());
            }
            let id = preserve(tx, u, &g, &local, time)?;
            remember(tx, u, &op, &fingerprint, id)
        }
        Action::Preserve {
            game: g,
            slots,
            local,
            time,
            op,
        } => {
            let mut g = game(tx, u, &g)?;
            // A disconnected session can contain progress for a slot since removed.
            // Its backup may use historical IDs while the published head stays on the current recipe.
            let historical = slots.iter().any(|name| !g.recipe.slots.contains_key(name));
            if historical {
                let retired = retired_slots(tx, u, &g.recipe.game)?;
                for name in &slots {
                    if let Some(kind) = retired.get(name) {
                        g.recipe.slots.insert(
                            name.clone(),
                            Slot {
                                kind: kind.clone(),
                                codec: raw_codec(),
                                ignores: Default::default(),
                                settings: false,
                                source: None,
                                locations: Default::default(),
                            },
                        );
                    }
                }
            }
            let mut scope = g.recipe.clone();
            ensure!(
                slots.iter().all(|k| scope.slots.contains_key(k)),
                "unknown candidate slot"
            );
            scope.slots.retain(|k, _| slots.contains(k));
            local.validate(&scope)?;
            let prior = g
                .head
                .map(|id| version(tx, u, &g.recipe.game, id))
                .transpose()?
                .map(|v| v.tree)
                .unwrap_or_default();
            let full = prior.overlay(&scope, &local);
            let id = if historical {
                snapshot(tx, u, &g, &full, time, "offline", false)?
            } else {
                preserve(tx, u, &g, &full, time)?
            };
            remember(tx, u, &op, &fingerprint, id)
        }
        Action::BackupChanges { game, id } => Ok(json!(backup_changes(tx, u, &game, id)?)),
        Action::Backups { game: g } => {
            let state = game(tx, u, &g)?;
            let mut stmt=tx.prepare("SELECT id,origin,created,pin FROM versions WHERE account=? AND game=? ORDER BY id DESC")?;
            let rows = stmt.query_map(params![u.account, g], |r| {
                let id = r.get(0)?;
                Ok(Backup {
                    id,
                    origin: r.get(1)?,
                    time: r.get(2)?,
                    reason: r.get(3)?,
                    head: state.head == Some(id),
                })
            })?;
            Ok(json!(rows.collect::<rusqlite::Result<Vec<_>>>()?))
        }
        Action::Pin { game: g, id } => {
            ensure!(tx.execute("UPDATE versions SET pin=CASE WHEN pin='' THEN 'manual' ELSE pin END WHERE account=? AND game=? AND id=?",params![u.account,g,id])?==1,"backup unavailable");
            Ok(Value::Null)
        }
        Action::Delete { game: g, id } => {
            ensure!(game(tx, u, &g)?.head != Some(id), "current head is in use");
            ensure!(
                tx.execute(
                    "DELETE FROM versions WHERE account=? AND game=? AND id=?",
                    params![u.account, g, id]
                )? == 1,
                "backup unavailable"
            );
            Ok(Value::Null)
        }
        Action::UnpinCurrent { game: g, id } => {
            if game(tx, u, &g)?.head != Some(id) {
                return Err(stale());
            }
            tx.execute(
                "UPDATE versions SET pin='' WHERE account=? AND game=? AND id=?",
                params![u.account, g, id],
            )?;
            Ok(Value::Null)
        }
        Action::ReadRevision { game: g, id } => Ok(json!(version(tx, u, &g, id)?)),
        Action::Devices => {
            let mut stmt =
                tx.prepare("SELECT id,name FROM devices WHERE account=? ORDER BY name")?;
            let rows = stmt.query_map([&u.account], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?;
            Ok(json!(rows.collect::<rusqlite::Result<Vec<_>>>()?))
        }
        Action::Revoke { device } => {
            ensure!(
                tx.execute(
                    "DELETE FROM devices WHERE account=? AND id=?",
                    params![u.account, device]
                )? == 1,
                "device unavailable"
            );
            Ok(Value::Null)
        }
    }
}
