//! Coordinate an explicit executable update with active game sessions.
use crate::{distribution, local::Local, model::Credentials};
use anyhow::{Context, Result, bail, ensure};
use std::{
    fs::{self, File, OpenOptions},
    io::ErrorKind,
    path::{Path, PathBuf},
    process::Command,
};

fn lock(executable: &Path, exclusive: bool) -> Result<Option<File>> {
    let name = executable
        .file_name()
        .context("missing Buddy executable name")?
        .to_string_lossy();
    let path = executable.with_file_name(format!(".{name}.update.lock"));
    let f = match OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
    {
        Ok(f) => f,
        Err(e)
            if !exclusive
                && matches!(
                    e.kind(),
                    ErrorKind::PermissionDenied | ErrorKind::ReadOnlyFilesystem
                ) =>
        {
            // Other users can share an existing lock for a system-managed installation.
            match File::open(&path) {
                Ok(f) => f,
                Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::PermissionDenied) => {
                    return Ok(None);
                }
                Err(e) => return Err(e).context("open shared Buddy update lock"),
            }
        }
        Err(e) => return Err(e).context("open Buddy update lock"),
    };
    if exclusive {
        f.try_lock()
            .context("finish active games and other Buddy updates before updating")?;
    } else {
        f.try_lock_shared()
            .context("Buddy is updating; try Play again in a moment")?;
    }
    Ok(Some(f))
}

pub fn game_guard() -> Result<Option<File>> {
    lock(&std::env::current_exe()?, false)
}

pub enum Outcome {
    Current,
    Updated(PathBuf),
    Cancelled,
}

pub fn run(
    local: &Local,
    credentials: &Credentials,
    trust_key: Option<&str>,
    confirm: impl FnOnce() -> Result<bool>,
) -> Result<Outcome> {
    let exe = std::env::current_exe()?
        .canonicalize()
        .context("reopen the installed copy of Buddy before updating")?;
    let installed = blake3::Hasher::new()
        .update_reader(File::open(&exe)?)?
        .finalize();
    let Some(file) = distribution::prepare_update(local, credentials, &exe, trust_key)? else {
        return Ok(Outcome::Current);
    };
    if !confirm()? {
        return Ok(Outcome::Cancelled);
    }
    let _guard = lock(&exe, true)?;
    let current = blake3::Hasher::new()
        .update_reader(File::open(&exe)?)?
        .finalize();
    ensure!(
        current == installed,
        "Buddy changed during download; reopen Buddy and check again"
    );
    ensure!(
        local.scope(&local.credentials()?) == local.scope(credentials),
        "account changed during update; reopen Buddy and check again"
    );
    distribution::check_prepared_update(local, credentials, file.path())?;
    replace(&exe, file)?;
    Ok(Outcome::Updated(exe))
}

#[cfg(unix)]
fn replace(exe: &Path, file: tempfile::NamedTempFile) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = fs::metadata(exe)?.permissions().mode() & 0o777;
    file.as_file()
        .set_permissions(fs::Permissions::from_mode(mode))?;
    file.as_file().sync_all()?;
    file.persist(exe).context("replace Buddy executable")?;
    File::open(
        exe.parent()
            .context("missing Buddy installation directory")?,
    )?
    .sync_all()?;
    Ok(())
}

#[cfg(windows)]
fn replace(exe: &Path, file: tempfile::NamedTempFile) -> Result<()> {
    // Windows moves its running image aside before installing the replacement.
    // Retain a complete original on disk until that sequence has succeeded.
    let mut backup = tempfile::Builder::new()
        .prefix("buddy-previous-")
        .suffix(".exe")
        .tempfile_in(
            exe.parent()
                .context("missing Buddy installation directory")?,
        )?;
    std::io::copy(&mut File::open(exe)?, backup.as_file_mut())?;
    backup.as_file().sync_all()?;
    let backup = backup.into_temp_path().keep()?;
    if let Err(e) = self_replace::self_replace(file.path()) {
        if !exe.try_exists()?
            && let Err(restore) = fs::rename(&backup, exe)
        {
            bail!(
                "update failed ({e}); restore {} to {} ({restore})",
                backup.display(),
                exe.display()
            );
        }
        if backup.exists() {
            fs::remove_file(&backup)?;
        }
        return Err(e).context("replace Buddy executable");
    }
    fs::remove_file(backup)?;
    Ok(())
}

pub fn restart_gui(local: &Local, exe: &Path) -> Result<()> {
    let mut command = Command::new(exe);
    if local.explicit_home {
        command.arg("--profile-dir").arg(&local.home);
    }
    match command.arg("gui").spawn() {
        Ok(_) => Ok(()),
        Err(e) => bail!("update installed; reopen {} ({e})", exe.display()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn updates_wait_for_all_game_wrappers_using_the_installation() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("Buddy");
        let first = lock(&exe, false).unwrap();
        let second = lock(&exe, false).unwrap();
        assert!(lock(&exe, true).is_err());
        drop(first);
        assert!(lock(&exe, true).is_err());
        drop(second);
        let updating = lock(&exe, true).unwrap();
        assert!(lock(&exe, false).is_err());
        drop(updating);
        assert!(lock(&exe, false).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn replacement_preserves_executable_permissions() {
        use std::{io::Write, os::unix::fs::PermissionsExt};
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("buddy");
        fs::write(&exe, b"previous").unwrap();
        fs::set_permissions(&exe, fs::Permissions::from_mode(0o750)).unwrap();
        let mut next = tempfile::NamedTempFile::new_in(dir.path()).unwrap();
        next.write_all(b"new").unwrap();
        replace(&exe, next).unwrap();
        assert_eq!(fs::read(&exe).unwrap(), b"new");
        assert_eq!(
            fs::metadata(exe).unwrap().permissions().mode() & 0o777,
            0o750
        );
    }
}
