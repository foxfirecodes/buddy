//! The host operator publishes local files; HTTP only serves immutable snapshots.
use super::*;
use crate::distribution::{
    MAX_BINARY, Release, SignedRelease, hash_copy, platform_ok, verify_platform,
};
use axum::{body::Body, http::header};
use ring::{
    rand::SystemRandom,
    signature::{Ed25519KeyPair, KeyPair},
};
use std::io;
use tempfile::NamedTempFile;
use tokio_util::io::ReaderStream;

fn directory(store: &Store) -> PathBuf {
    let mut path = store.0.as_os_str().to_owned();
    path.push(".releases");
    path.into()
}

fn signing_key(dir: &Path) -> Result<Ed25519KeyPair> {
    let path = dir.join("signing-key.pk8");
    if !path.try_exists()? {
        let bytes = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new())
            .map_err(|_| anyhow!("could not generate release signing key"))?;
        let mut temp = NamedTempFile::new_in(dir)?;
        temp.write_all(bytes.as_ref())?;
        temp.as_file().sync_all()?;
        if let Err(e) = temp.persist_noclobber(&path)
            && e.error.kind() != io::ErrorKind::AlreadyExists
        {
            return Err(e.error.into());
        }
    }
    Ed25519KeyPair::from_pkcs8(&fs::read(path)?).map_err(|_| anyhow!("invalid release signing key"))
}

pub fn publish_release(config: &Path, platform: &str, binary: &Path) -> Result<String> {
    ensure!(
        platform_ok(platform),
        "choose linux-x86_64 or windows-x86_64"
    );
    let (store, _) = Store::open(config)?;
    let dir = directory(&store);
    if !dir.try_exists()? {
        match fs::create_dir(&dir) {
            Ok(()) => (),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => (),
            Err(e) => return Err(e.into()),
        }
    }
    ensure!(
        fs::symlink_metadata(&dir)?.is_dir(),
        "invalid release directory"
    );
    crate::host::protect_dir(&dir)?;
    let key = signing_key(&dir)?;
    let mut staged = NamedTempFile::new_in(&dir)?;
    let (sha256, size) = hash_copy(
        fs::File::open(binary).context("open client executable")?,
        &mut staged,
        MAX_BINARY,
    )?;
    verify_platform(staged.path(), platform)?;
    staged.as_file().sync_all()?;
    let destination = dir.join(&sha256);
    match staged.persist_noclobber(&destination) {
        Ok(_) => (),
        Err(e) if e.error.kind() == io::ErrorKind::AlreadyExists => {
            ensure!(
                hash_copy(fs::File::open(&destination)?, io::sink(), MAX_BINARY)?.0 == sha256,
                "existing release file is damaged"
            );
        }
        Err(e) => return Err(e.error.into()),
    }
    #[cfg(unix)]
    fs::File::open(&dir)?.sync_all()?;
    let mut db = store.connection()?;
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let previous_manifest: Option<String> = tx
        .query_row("SELECT manifest FROM releases LIMIT 1", [], |row| {
            row.get(0)
        })
        .optional()?;
    let prior: Option<(i64, String, String)> = tx.query_row(
        "SELECT generation, sha256, manifest FROM releases WHERE platform=? ORDER BY generation DESC LIMIT 1",
        [platform], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional()?;
    let signer = hex::encode(key.public_key().as_ref());
    let installer =
        crate::distribution::installer_script(platform).context("unsupported install target")?;
    let installer_sha256 = hex::encode(ring::digest::digest(
        &ring::digest::SHA256,
        installer.as_bytes(),
    ));
    if let Some(manifest) = previous_manifest {
        let previous: SignedRelease = serde_json::from_str(&manifest)?;
        ensure!(
            previous.signer == signer,
            "restore the original release signing key before publishing"
        );
    }
    let generation = match &prior {
        Some((generation, hash, previous))
            if hash == &sha256
                && serde_json::from_str::<SignedRelease>(previous)?
                    .release
                    .installer_sha256
                    == installer_sha256 =>
        {
            *generation
        }
        previous => {
            let generation = previous.as_ref().map_or(1, |(n, _, _)| n + 1);
            let release = Release {
                platform: platform.to_owned(),
                generation,
                size,
                sha256,
                installer_sha256,
            };
            let signature = hex::encode(key.sign(&release.signing_bytes()?).as_ref());
            let manifest = SignedRelease {
                release,
                signer: signer.clone(),
                signature,
            };
            tx.execute("INSERT INTO releases(platform,generation,sha256,installer_hash,installer,manifest) VALUES(?,?,?,?,?,?)",
                params![platform, generation, manifest.release.sha256, manifest.release.installer_sha256, installer, serde_json::to_string(&manifest)?])?;
            generation
        }
    };
    tx.commit()?;
    Ok(format!(
        "{platform} release {generation}\nRelease signing key: {signer}"
    ))
}

pub(super) fn routes() -> Router<Store> {
    Router::new()
        .route("/install/{hash}", get(installer))
        .route("/v1/device", get(identity))
        .route("/v1/releases/{platform}", get(latest))
        .route("/v1/releases/files/{hash}", get(binary))
        .route("/v1/device-links", post(issue))
        .route("/v1/device-links/redeem", post(redeem))
        .layer(DefaultBodyLimit::max(4096))
}

async fn latest(
    State(store): State<Store>,
    UrlPath(platform): UrlPath<String>,
) -> ApiResult<Json<SignedRelease>> {
    database(move || {
        ensure!(platform_ok(&platform), "unsupported release target");
        let json: String = store
            .connection()?
            .query_row(
                "SELECT manifest FROM releases WHERE platform=? ORDER BY generation DESC LIMIT 1",
                [&platform],
                |r| r.get(0),
            )
            .optional()?
            .ok_or_else(|| {
                denied(
                    StatusCode::NOT_FOUND,
                    &format!(
                        "on the host, run buddy publish-release --config server.toml {platform} {}",
                        if platform == "linux-x86_64" {
                            "/path/to/portable/buddy"
                        } else {
                            "PATH_TO_BUDDY.exe"
                        }
                    ),
                )
            })?;
        Ok(Json(serde_json::from_str(&json)?))
    })
    .await
}

async fn binary(State(store): State<Store>, UrlPath(hash): UrlPath<String>) -> ApiResult<Response> {
    let (file, size) = database(move || {
        ensure!(hash_ok(&hash), "invalid release digest");
        let published: bool = store.connection()?.query_row(
            "SELECT EXISTS(SELECT 1 FROM releases WHERE sha256=?)",
            [&hash],
            |r| r.get(0),
        )?;
        if !published {
            return Err(denied(StatusCode::NOT_FOUND, "release unavailable"));
        }
        let file = fs::File::open(directory(&store).join(hash))?;
        let len = file.metadata()?.len();
        Ok((file, len))
    })
    .await?;
    let stream = ReaderStream::new(tokio::fs::File::from_std(file));
    Ok((
        [
            (header::CONTENT_TYPE, "application/octet-stream".to_string()),
            (header::CONTENT_LENGTH, size.to_string()),
        ],
        Body::from_stream(stream),
    )
        .into_response())
}

async fn issue(State(store): State<Store>, headers: HeaderMap) -> ApiResult<impl IntoResponse> {
    let code = database(move || {
        let mut db = store.connection()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let user = who(&tx, &bearer(&headers))?;
        let now = now();
        tx.execute("DELETE FROM device_links WHERE expires<=?", [now])?;
        let code = format!("{}{}", id(), id());
        tx.execute(
            "INSERT INTO device_links(hash,account,expires) VALUES(?,?,?)",
            params![digest(code.as_bytes()), user.account, now + 600],
        )?;
        tx.commit()?;
        Ok(code)
    })
    .await?;
    Ok(([(header::CACHE_CONTROL, "no-store")], Json(code)))
}

#[derive(Deserialize)]
struct Claim {
    code: String,
    device: String,
    // Only the device knows its new bearer token.
    token_hash: String,
}

async fn redeem(State(store): State<Store>, Json(claim): Json<Claim>) -> ApiResult<StatusCode> {
    database(move || {
        ensure!(
            hash_ok(&claim.code)
                && hash_ok(&claim.token_hash)
                && !claim.device.trim().is_empty()
                && claim.device.len() <= 120,
            "invalid device link"
        );
        let mut db = store.connection()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let account: String = tx
            .query_row(
                "DELETE FROM device_links WHERE hash=? AND expires>? RETURNING account",
                params![digest(claim.code.as_bytes()), now()],
                |r| r.get(0),
            )
            .optional()?
            .ok_or_else(|| {
                denied(
                    StatusCode::UNAUTHORIZED,
                    "device code was used or expired; generate another install command",
                )
            })?;
        tx.execute(
            "INSERT INTO devices(id,account,name,token) VALUES(?,?,?,?)",
            params![id(), account, claim.device, claim.token_hash],
        )?;
        tx.commit()?;
        Ok(StatusCode::NO_CONTENT)
    })
    .await
}

async fn installer(
    State(store): State<Store>,
    UrlPath(hash): UrlPath<String>,
) -> ApiResult<impl IntoResponse> {
    let script: String = database(move || {
        ensure!(hash_ok(&hash), "invalid installer digest");
        store
            .connection()?
            .query_row(
                "SELECT installer FROM releases WHERE installer_hash=? LIMIT 1",
                [hash],
                |r| r.get(0),
            )
            .optional()?
            .ok_or_else(|| denied(StatusCode::NOT_FOUND, "installer unavailable"))
    })
    .await?;
    Ok((
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        script,
    ))
}

async fn identity(State(store): State<Store>, headers: HeaderMap) -> ApiResult<Json<String>> {
    database(move || Ok(Json(who(&store.connection()?, &bearer(&headers))?.account))).await
}
