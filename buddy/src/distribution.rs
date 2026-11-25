//! Account-bound device enrollment and host-signed, per-platform client builds.
use crate::{
    local::{Local, canonical_url, checked, default_device, http},
    model::{Credentials, digest, hash_ok, id, key},
};
use anyhow::{Context, Result, anyhow, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use object::Object;
use ring::{digest as sha, signature};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    collections::BTreeMap,
    fs,
    io::{self, Read, Write},
    path::Path,
};
use tempfile::NamedTempFile;

pub(crate) const MAX_BINARY: u64 = 512 * 1024 * 1024;
const METADATA_LIMIT: u64 = 32768;

pub(crate) fn installer_script(platform: &str) -> Option<&'static str> {
    match platform {
        "linux-x86_64" => Some(include_str!("../assets/install/linux.sh")),
        "windows-x86_64" => Some(include_str!("../assets/install/windows.ps1")),
        _ => None,
    }
}
pub(crate) fn platform_ok(platform: &str) -> bool {
    installer_script(platform).is_some()
}
fn current_platform() -> Result<String> {
    let platform = format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH);
    ensure!(
        platform_ok(&platform),
        "Buddy updates currently support x64 Windows and Linux"
    );
    Ok(platform)
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct Release {
    pub platform: String,
    pub generation: i64,
    pub size: u64,
    pub sha256: String,
    pub installer_sha256: String,
}
impl Release {
    pub(crate) fn signing_bytes(&self) -> Result<Vec<u8>> {
        let mut bytes = b"Buddy client release v1\0".to_vec();
        serde_json::to_writer(&mut bytes, self)?;
        Ok(bytes)
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct SignedRelease {
    pub release: Release,
    pub signer: String,
    pub signature: String,
}
impl SignedRelease {
    pub(crate) fn verify(&self, platform: &str) -> Result<()> {
        let r = &self.release;
        ensure!(
            platform_ok(platform)
                && r.platform == platform
                && r.generation > 0
                && (1..=MAX_BINARY).contains(&r.size)
                && hash_ok(&r.sha256)
                && hash_ok(&r.installer_sha256)
                && hash_ok(&self.signer),
            "invalid release manifest for {platform}"
        );
        let key = hex::decode(&self.signer)?;
        let sig = hex::decode(&self.signature)?;
        signature::UnparsedPublicKey::new(&signature::ED25519, key)
            .verify(&r.signing_bytes()?, &sig)
            .map_err(|_| anyhow!("release signature verification failed"))?;
        Ok(())
    }
}

pub(crate) fn hash_copy(
    mut reader: impl Read,
    mut writer: impl Write,
    limit: u64,
) -> Result<(String, u64)> {
    let mut hash = sha::Context::new(&sha::SHA256);
    let mut buffer = [0u8; 65536];
    let mut size = 0u64;
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        size += count as u64;
        ensure!(
            size <= limit,
            "release exceeds advertised size or the 512 MiB limit"
        );
        hash.update(&buffer[..count]);
        writer.write_all(&buffer[..count])?;
    }
    Ok((hex::encode(hash.finish()), size))
}
pub(crate) fn verify_platform(path: &Path, platform: &str) -> Result<()> {
    use object::{Architecture, BinaryFormat, ObjectKind};
    let cache = object::read::ReadCache::new(fs::File::open(path)?);
    let file = object::File::parse(&cache).context("expected a native executable")?;
    ensure!(
        file.architecture() == Architecture::X86_64
            && file.is_64()
            && file.entry() != 0
            && match platform {
                "linux-x86_64" =>
                    file.format() == BinaryFormat::Elf
                        && matches!(file.kind(), ObjectKind::Executable | ObjectKind::Dynamic),
                "windows-x86_64" =>
                    file.format() == BinaryFormat::Pe && file.kind() == ObjectKind::Executable,
                _ => false,
            },
        "the executable does not match {platform}"
    );
    Ok(())
}

fn small_json<T: DeserializeOwned>(response: reqwest::blocking::Response) -> Result<T> {
    let mut buf = vec![];
    checked(response)?
        .take(METADATA_LIMIT + 1)
        .read_to_end(&mut buf)?;
    ensure!(
        buf.len() <= METADATA_LIMIT as usize,
        "response exceeds size limit"
    );
    Ok(serde_json::from_slice(&buf)?)
}
fn latest(c: &Credentials, platform: &str) -> Result<SignedRelease> {
    ensure!(
        platform_ok(platform),
        "choose linux-x86_64 or windows-x86_64"
    );
    let response = http(c.paired_ca.as_deref())?
        .get(format!("{}/v1/releases/{platform}", c.url))
        .send()?;
    let release: SignedRelease = small_json(response)?;
    release.verify(platform)?;
    Ok(release)
}

pub(crate) fn comparison_code(public_key: &[u8]) -> String {
    sha::digest(&sha::SHA256, public_key).as_ref()[..10]
        .chunks_exact(2)
        .map(hex::encode_upper)
        .collect::<Vec<_>>()
        .join(" ")
}

#[derive(Clone, Debug)]
pub(crate) struct UntrustedReleaseKey {
    pub signer: String,
    pub code: String,
    pub server: String,
    scope: String,
}
impl UntrustedReleaseKey {
    pub fn confirmed_key(&self, local: &Local, credentials: &Credentials) -> Result<&str> {
        ensure!(
            self.scope == local.scope(credentials),
            "account or server changed; compare the key again"
        );
        Ok(&self.signer)
    }
}
impl std::fmt::Display for UntrustedReleaseKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "compare {} with the verification code from buddy server status on {}; for unattended use, supply the host's public key with --trust-key KEY",
            self.code, self.server
        )
    }
}
impl std::error::Error for UntrustedReleaseKey {}

#[derive(Serialize, Deserialize)]
struct ReleaseTrust {
    key: String,
    // Retain the identity of the newest signed release independently for each target.
    latest: BTreeMap<String, Release>,
}
fn trust(
    local: &Local,
    c: &Credentials,
    manifest: &SignedRelease,
    expected: Option<&str>,
) -> Result<()> {
    let scope = format!("release-trust/{}", local.scope(c));
    let _lock = local.lock(&scope)?;
    let mut trust = local.read::<ReleaseTrust>(&scope)?;
    if let Some(pin) = expected {
        ensure!(
            pin.trim().eq_ignore_ascii_case(&manifest.signer),
            "release signing key differs from the trusted host key"
        );
    }
    if let Some(previous) = &trust {
        ensure!(
            previous.key == manifest.signer,
            "the server release signing key changed; verify the server identity"
        );
    } else {
        if expected.is_none() && !c.url.starts_with("https://") {
            return Err(UntrustedReleaseKey {
                code: comparison_code(&hex::decode(&manifest.signer)?),
                signer: manifest.signer.clone(),
                server: c.url.clone(),
                scope: local.scope(c),
            }
            .into());
        }
        trust = Some(ReleaseTrust {
            key: manifest.signer.clone(),
            latest: Default::default(),
        });
    }
    let mut trust = trust.unwrap();
    let release = &manifest.release;
    if let Some(prior) = trust.latest.get(&release.platform) {
        ensure!(
            release.generation > prior.generation || release == prior,
            "server provided an older or conflicting release"
        );
        if prior == release {
            return Ok(());
        }
    }
    trust
        .latest
        .insert(release.platform.clone(), release.clone());
    local.write(&scope, &trust)
}

#[derive(Serialize, Deserialize)]
struct Enrollment {
    url: String,
    account: String,
    ca: Option<String>,
    code: String,
    build: SignedRelease,
}

pub fn install_command(
    local: &Local,
    c: &Credentials,
    platform: &str,
    trust_key: Option<&str>,
) -> Result<String> {
    let manifest = latest(c, platform)?;
    trust(local, c, &manifest, trust_key)?;
    let code: String = small_json(
        http(c.paired_ca.as_deref())?
            .post(format!("{}/v1/device-links", c.url))
            .bearer_auth(&c.token)
            .send()?,
    )?;
    ensure!(hash_ok(&code), "invalid device code from server");
    command(&Enrollment {
        url: c.url.clone(),
        account: c.account.clone(),
        ca: c.paired_ca.clone(),
        code,
        build: manifest,
    })
}

pub(crate) fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}
fn powershell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}
fn command(e: &Enrollment) -> Result<String> {
    let payload = STANDARD.encode(serde_json::to_vec(e)?);
    let r = &e.build.release;
    let script_hash = &r.installer_sha256;
    let install_url = format!("{}/install/{script_hash}", e.url);
    let binary_url = format!("{}/v1/releases/files/{}", e.url, r.sha256);
    let ca = e.ca.as_ref().map(|ca| STANDARD.encode(ca));
    if r.platform == "linux-x86_64" {
        let setup_ca = match ca {
            Some(ca) => format!(
                "printf %s '{ca}' | base64 -d > \"$t/ca\" || exit 1; set -- --cacert \"$t/ca\"; ca=$t/ca"
            ),
            None => "set --; ca=''".into(),
        };
        Ok(format!(
            "(umask 077; t=$(mktemp -d) || exit 1; trap 'rm -f -- \"$t/setup\" \"$t/ca\"; rmdir -- \"$t\"' EXIT; {setup_ca}; curl -fsS --proto '=http,https' --max-filesize 65536 \"$@\" -o \"$t/setup\" -- {} && printf '%s  %s\\n' '{script_hash}' \"$t/setup\" | sha256sum -c --status - && printf '%s\\n' '{payload}' | sh \"$t/setup\" {} '{}' {} \"$ca\")",
            shell_quote(&install_url),
            shell_quote(&binary_url),
            r.sha256,
            r.size
        ))
    } else {
        let setup_ca = match ca {
            Some(ca) => format!(
                "$ca=Join-Path $t 'ca.pem'; [IO.File]::WriteAllBytes($ca,[Convert]::FromBase64String('{ca}')); $tls=@('--cacert',$ca)"
            ),
            None => "$ca=''; $tls=@()".into(),
        };
        Ok(format!(
            "& {{ $ErrorActionPreference='Stop'; $t=Join-Path ([IO.Path]::GetTempPath()) ([Guid]::NewGuid().ToString('N')); [IO.Directory]::CreateDirectory($t) | Out-Null; $s=Join-Path $t 'setup.ps1'; try {{ {setup_ca}; & curl.exe -fsS --proto '=http,https' --max-filesize 65536 @tls -o $s -- {}; if ($LASTEXITCODE -ne 0 -or (Get-FileHash -LiteralPath $s -Algorithm SHA256).Hash -ne '{script_hash}') {{ throw 'Installer verification failed' }}; & ([ScriptBlock]::Create([IO.File]::ReadAllText($s))) {} '{}' {} $ca '{payload}' }} finally {{ if ([IO.File]::Exists($s)) {{ [IO.File]::Delete($s) }}; if ($ca -and [IO.File]::Exists($ca)) {{ [IO.File]::Delete($ca) }}; [IO.Directory]::Delete($t) }} }}",
            powershell_quote(&install_url),
            powershell_quote(&binary_url),
            r.sha256,
            r.size
        ))
    }
}

#[derive(Serialize, Deserialize)]
struct PendingEnrollment {
    code_hash: String,
    record: Credentials,
}

fn account_for(c: &Credentials) -> Result<String> {
    small_json(
        http(c.paired_ca.as_deref())?
            .get(format!("{}/v1/device", c.url))
            .bearer_auth(&c.token)
            .send()?,
    )
}

pub fn redeem_stdin(local: &Local, private_file: bool) -> Result<()> {
    let mut input = String::new();
    io::stdin()
        .take(METADATA_LIMIT + 1)
        .read_to_string(&mut input)?;
    ensure!(
        input.len() <= METADATA_LIMIT as usize,
        "device link exceeds size limit"
    );
    let enrollment: Enrollment = serde_json::from_slice(&STANDARD.decode(input.trim())?)?;
    redeem_device(local, enrollment, private_file)
}

fn redeem_device(local: &Local, enrollment: Enrollment, private_file: bool) -> Result<()> {
    let _lock = local.lock("device-enrollment")?;
    let Enrollment {
        url,
        account,
        ca,
        code,
        build,
    } = enrollment;
    ensure!(
        canonical_url(&url)? == url && key(&account) && hash_ok(&code),
        "invalid enrollment"
    );
    ensure!(
        ca.is_none() || url.starts_with("https://"),
        "a paired server requires HTTPS"
    );
    build.verify(&current_platform()?)?;
    let code_hash = digest(code.as_bytes());
    if local.read::<Credentials>("login")?.is_some() {
        let c = local.credentials()?;
        ensure!(
            c.url == url && c.account == account && c.paired_ca == ca,
            "this profile is signed in to a different account or server"
        );
        ensure!(
            account_for(&c)? == account,
            "this profile has a different account"
        );
        trust(local, &c, &build, Some(&build.signer))?;
        return Ok(());
    }
    let pending = local
        .read::<PendingEnrollment>("pending-enrollment")?
        .filter(|p| {
            p.code_hash == code_hash
                && p.record.url == url
                && p.record.account == account
                && p.record.paired_ca == ca
        });
    // Prepare durable credentials before spending the grant. Reuse the same device's
    // token after a lost response or interruption between redemption and activation.
    let (c, record, retry) = if let Some(p) = pending {
        (local.unlock_credential(p.record.clone())?, p.record, true)
    } else {
        let c = Credentials {
            url,
            account,
            device: default_device(),
            token: format!("{}{}", id(), id()),
            paired_ca: ca,
        };
        let record = match local.stored_credential(&c, private_file) {
            Ok(record) => record,
            Err(error) if !private_file => {
                let can_prompt = cfg!(windows)
                    || std::env::var_os("DISPLAY").is_some()
                    || std::env::var_os("WAYLAND_DISPLAY").is_some();
                if can_prompt && crate::ui::choose("Device credentials",
                    "The OS credential store is unavailable. Save this device token in Buddy’s private profile?",
                    &["Cancel", "Use private profile"]).ok() == Some(1) {
                    local.stored_credential(&c, true)?
                } else {
                    return Err(error.context("device code is still available; unlock your credential store or explicitly set BUDDY_ALLOW_FILE_CREDENTIAL=1 and retry"));
                }
            }
            Err(error) => return Err(error),
        };
        local.write(
            "pending-enrollment",
            &PendingEnrollment {
                code_hash,
                record: record.clone(),
            },
        )?;
        (c, record, false)
    };
    trust(local, &c, &build, Some(&build.signer))?;
    let already_redeemed = if retry {
        match account_for(&c) {
            Ok(account) => {
                ensure!(account == c.account, "unexpected enrollment account");
                true
            }
            Err(e)
                if e.downcast_ref::<crate::local::ApiError>()
                    .is_some_and(|e| e.0 == 401) =>
            {
                false
            }
            Err(e) => return Err(e),
        }
    } else {
        false
    };
    if !already_redeemed {
        let response = http(c.paired_ca.as_deref())?.post(format!("{}/v1/device-links/redeem", c.url))
            .json(&serde_json::json!({"code":code, "device":c.device, "token_hash":digest(c.token.as_bytes())}))
            .send()?;
        checked(response)?;
        ensure!(
            account_for(&c)? == c.account,
            "device link belongs to another account"
        );
    }
    local.finish_enrollment(&record)
}

pub fn prepare_update(
    local: &Local,
    c: &Credentials,
    target_exe: &Path,
    trust_key: Option<&str>,
) -> Result<Option<NamedTempFile>> {
    let platform = current_platform()?;
    let manifest = latest(c, &platform)?;
    trust(local, c, &manifest, trust_key)?;
    let r = &manifest.release;
    if fs::metadata(target_exe)?.len() == r.size
        && hash_copy(fs::File::open(target_exe)?, io::sink(), r.size)?.0 == r.sha256
    {
        return Ok(None);
    }
    let response = checked(
        http(c.paired_ca.as_deref())?
            .get(format!("{}/v1/releases/files/{}", c.url, r.sha256))
            .send()?,
    )?;
    if let Some(len) = response.content_length() {
        ensure!(
            len == r.size,
            "downloaded release length differs from signed manifest"
        );
    }
    let mut staged = tempfile::Builder::new()
        .prefix(".buddy-update-")
        .suffix(if cfg!(windows) { ".exe" } else { "" })
        .tempfile_in(
            target_exe
                .parent()
                .context("executable needs a parent directory")?,
        )?;
    let (digest, size) = hash_copy(response, &mut staged, r.size)?;
    ensure!(
        digest == r.sha256 && size == r.size,
        "downloaded release failed integrity verification"
    );
    verify_platform(staged.path(), &platform)?;
    staged.as_file().sync_all()?;
    Ok(Some(staged))
}

pub fn check_prepared_update(local: &Local, c: &Credentials, staged: &Path) -> Result<()> {
    let scope = format!("release-trust/{}", local.scope(c));
    let trust = local
        .read::<ReleaseTrust>(&scope)?
        .context("this account has not approved an update")?;
    let r = trust
        .latest
        .get(&current_platform()?)
        .context("a new update review is required")?;
    let (hash, size) = hash_copy(fs::File::open(staged)?, io::sink(), r.size)?;
    ensure!(
        hash == r.sha256 && size == r.size,
        "a newer release was checked or the download changed; check for updates again"
    );
    Ok(())
}
