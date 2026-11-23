//! Steam expands %command% for store titles. A shortcut has separate Target and Launch Options fields.
use anyhow::{Context, Result, ensure};
use std::path::Path;

pub struct LaunchFields {
    pub target: String,
    pub options: String,
    pub store_title: bool,
}

impl LaunchFields {
    pub fn new(buddy: &Path, profile: Option<&Path>, game: &str) -> Result<Self> {
        ensure!(crate::model::key(game), "invalid game ID");
        let store_title = game
            .strip_prefix("steam-")
            .is_some_and(|id| id.parse::<u32>().is_ok());
        let target = quoted_path(buddy, store_title)?;
        let mut options = if store_title {
            format!("{target} ")
        } else {
            String::new()
        };
        if let Some(path) = profile {
            options.push_str(&format!(
                "--profile-dir {} ",
                quoted_path(path, store_title)?
            ));
        }
        options.push_str(&format!("run --game {game} -- "));
        if store_title {
            options.push_str("%command%");
        }
        Ok(Self {
            target,
            options,
            store_title,
        })
    }
}
fn quoted_path(p: &Path, shell: bool) -> Result<String> {
    let s = p
        .to_str()
        .context("Steam launch fields need a Unicode path")?;
    ensure!(
        !s.contains(['\n', '\r', '"']),
        "Steam launch fields cannot represent this path"
    );
    // Linux runs the store launch template through a shell. The shortcut Target
    // field and the Windows command line both take the literal executable path.
    if shell && cfg!(unix) {
        return Ok(format!("'{}'", s.replace('\'', "'\\''")));
    }
    #[cfg(windows)]
    let s = {
        let trailing = s.chars().rev().take_while(|&c| c == '\\').count();
        format!("{s}{}", "\\".repeat(trailing))
    };
    Ok(format!("\"{s}\""))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shortcut_needs_original_target_and_args() {
        let f = LaunchFields::new(
            Path::new("Buddy Tools/buddy"),
            Some(Path::new("Trial Profile")),
            "shortcut-42",
        )
        .unwrap();
        assert_eq!(f.target, "\"Buddy Tools/buddy\"");
        assert_eq!(
            f.options,
            "--profile-dir \"Trial Profile\" run --game shortcut-42 -- "
        );
        assert!(!f.store_title);
    }
    #[test]
    fn store_expands_steam_command_once() {
        let f = LaunchFields::new(Path::new("Buddy Tools/buddy"), None, "steam-42").unwrap();
        assert!(f.store_title);
        assert_eq!(
            f.options,
            format!("{} run --game steam-42 -- %command%", f.target)
        );
    }
    #[cfg(unix)]
    #[test]
    fn store_wrapper_path_is_one_literal_shell_word() {
        let f =
            LaunchFields::new(Path::new("/games/Jane's $folder/buddy"), None, "steam-42").unwrap();
        assert_eq!(f.target, "'/games/Jane'\\''s $folder/buddy'");
    }
}
