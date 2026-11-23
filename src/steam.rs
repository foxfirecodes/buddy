use std::collections::HashSet;
use std::env;
use std::fmt::Write;
use std::path::{Path, PathBuf};

use steamlocate::{SteamDir, library::Library};

// Inspect each registered library even if a different drive is unmounted.
pub fn inventory(app: Option<u32>) -> String {
    let installs = match env::var_os("BUDDY_STEAM_ROOT") {
        Some(path) => SteamDir::from_dir(Path::new(&path)).map(|dir| vec![dir]),
        None => steamlocate::locate_all(),
    };
    let mut output = String::new();
    let mut seen = HashSet::new();
    match installs {
        Ok(installs) => {
            for steam in installs {
                match steam.library_paths() {
                    Ok(paths) => {
                        for path in paths {
                            if seen.insert(path.canonicalize().unwrap_or_else(|_| path.clone())) {
                                describe_library(&mut output, &path, app);
                            }
                        }
                    }
                    Err(error) => {
                        let _ =
                            writeln!(output, "Library index {}: {error}", steam.path().display());
                    }
                }
            }
        }
        Err(error) => {
            let _ = writeln!(output, "Steam discovery: {error}");
        }
    }
    if let Some(context) = env::var_os("STEAM_COMPAT_DATA_PATH").filter(|p| !p.is_empty()) {
        let prefix = PathBuf::from(context).join("pfx");
        let _ = writeln!(
            output,
            "Launch-context prefix: {} (exists={})",
            prefix.display(),
            prefix.is_dir()
        );
    }
    output
}

fn describe_library(output: &mut String, path: &Path, app: Option<u32>) {
    match Library::from_dir(path) {
        Ok(lib) => {
            let _ = writeln!(
                output,
                "Library: {} ({} app manifests)",
                path.display(),
                lib.app_ids().len()
            );
            if let Some(id) = app {
                if let Some(result) = lib.app(id) {
                    match result {
                        Ok(a) => {
                            let _ = writeln!(
                                output,
                                "  App {id}: {}",
                                lib.resolve_app_dir(&a).display()
                            );
                        }
                        Err(e) => {
                            let _ = writeln!(output, "  App {id}: unreadable manifest ({e})");
                        }
                    }
                }
                let pfx = path
                    .join("steamapps/compatdata")
                    .join(id.to_string())
                    .join("pfx");
                if pfx.is_dir() {
                    let _ = writeln!(output, "  Prefix candidate: {}", pfx.display());
                }
            }
        }
        Err(e) => {
            let _ = writeln!(output, "Library: {} (unavailable: {e})", path.display());
        }
    }
}
