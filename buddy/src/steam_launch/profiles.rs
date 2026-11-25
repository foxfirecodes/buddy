use crate::{local::steam_installs, model::digest};
use anyhow::{Result, ensure};
use keyvalues_parser::Value;
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug)]
pub struct Profile {
    pub id: String,
    pub label: String,
    pub(super) root: PathBuf,
    pub(super) config: PathBuf,
}

fn property<'a, 's>(value: &'a Value<'s>, key: &str) -> Option<&'a Value<'s>> {
    let Value::Obj(object) = value else {
        return None;
    };
    let (_, values) = object.iter().find(|(k, _)| k.eq_ignore_ascii_case(key))?;
    values.first()
}

fn root_value<'s>(input: &'s str, key: &str) -> Option<Value<'s>> {
    let root = keyvalues_parser::parse(input.trim_start_matches('\u{feff}'))
        .ok()?
        .into_vdf();
    root.key.eq_ignore_ascii_case(key).then_some(root.value)
}

fn displayable(text: &str) -> Option<&str> {
    let text = text.trim();
    (!text.is_empty() && !text.chars().any(char::is_control)).then_some(text)
}

#[derive(Default)]
struct AccountNames {
    login: Option<String>,
    persona: Option<String>,
}

impl AccountNames {
    fn label(self, user: u32, config: &Path) -> String {
        let persona = self.persona.or_else(|| {
            let input = fs::read_to_string(config.join("localconfig.vdf")).ok()?;
            let root = root_value(&input, "UserLocalConfigStore")?;
            let friends = property(&root, "friends")?;
            Some(displayable(property(friends, "PersonaName")?.get_str()?)?.to_owned())
        });
        match (self.login, persona) {
            (Some(login), Some(persona)) if !login.eq_ignore_ascii_case(&persona) => {
                format!("{login} ({persona})")
            }
            (Some(login), _) => login,
            (None, Some(persona)) => persona,
            (None, None) => format!("Steam account {user}"),
        }
    }
}

fn account_names(steam: &Path) -> BTreeMap<u64, AccountNames> {
    let mut names = BTreeMap::new();
    let saved = fs::read_to_string(steam.join("config/loginusers.vdf")).unwrap_or_default();
    if let Some(root) = root_value(&saved, "users")
        && let Some(users) = root.get_obj()
    {
        for (key, entries) in users.iter() {
            let (Ok(id), [record]) = (key.parse(), entries.as_slice()) else {
                continue;
            };
            let name = |key| displayable(property(record, key)?.get_str()?).map(str::to_owned);
            names.insert(
                id,
                AccountNames {
                    login: name("AccountName"),
                    persona: name("PersonaName"),
                },
            );
        }
    }
    // Steam can retain the account name here after it forgets the login.
    let config = fs::read_to_string(steam.join("config/config.vdf")).unwrap_or_default();
    if let Some(root) = root_value(&config, "InstallConfigStore")
        && let Some(accounts) = ["Software", "Valve", "Steam", "Accounts"]
            .into_iter()
            .try_fold(&root, property)
            .and_then(Value::get_obj)
    {
        for (login, entries) in accounts.iter() {
            let [record] = entries.as_slice() else {
                continue;
            };
            if let Some(login) = displayable(login)
                && let Some(id) = property(record, "SteamID")
                    .and_then(Value::get_str)
                    .and_then(|s| s.parse().ok())
            {
                names
                    .entry(id)
                    .or_default()
                    .login
                    .get_or_insert_with(|| login.to_owned());
            }
        }
    }
    names
}

pub fn list() -> Result<Vec<Profile>> {
    collect_profiles(steam_installs())
}

fn collect_profiles(installs: Vec<steamlocate::SteamDir>) -> Result<Vec<Profile>> {
    // loginusers.vdf uses full public, individual Steam IDs. userdata uses their account numbers.
    const INDIVIDUAL_STEAM_ID_BASE: u64 = 76_561_197_960_265_728;
    let mut profiles = Vec::new();
    for install in installs {
        let root = install.path().canonicalize()?;
        let userdata = root.join("userdata");
        if !userdata.exists() {
            continue;
        }
        let mut names = account_names(&root);
        for entry in fs::read_dir(userdata)? {
            let entry = entry?;
            let Some(user) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<u32>().ok())
            else {
                continue;
            };
            let config = entry.path().join("config");
            if user == 0 || !config.is_dir() {
                continue;
            }
            profiles.push(Profile {
                id: format!(
                    "{user}-{}",
                    &digest(root.to_string_lossy().as_bytes())[..12]
                ),
                label: names
                    .remove(&(INDIVIDUAL_STEAM_ID_BASE + u64::from(user)))
                    .unwrap_or_default()
                    .label(user, &config),
                root: root.clone(),
                config: config.canonicalize()?,
            });
        }
    }
    profiles.sort_by(|a, b| a.id.cmp(&b.id));
    profiles.dedup_by(|a, b| a.id == b.id);
    profiles.sort_by(|a, b| a.label.cmp(&b.label));
    for group in profiles.chunk_by_mut(|a, b| a.label == b.label) {
        if group.len() > 1 {
            for p in group {
                let directory = p.config.parent().unwrap_or(&p.root);
                p.label = format!("{} · {}", p.label, directory.display());
            }
        }
    }
    ensure!(
        !profiles.is_empty(),
        "Sign in to Steam on this device first."
    );
    Ok(profiles)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_profiles_use_their_own_login_and_persona_and_keep_their_selector_ids() -> Result<()> {
        let fixture = tempfile::tempdir()?;
        let steam = fixture.path();
        let configs = steam.join("config");
        fs::create_dir(&configs)?;
        for id in [7, 9, 10, 13] {
            fs::create_dir_all(steam.join(format!("userdata/{id}/config")))?;
        }
        fs::write(
            configs.join("loginusers.vdf"),
            concat!(
                "\u{feff}",
                r#""users" {
            "76561197960265735" { "AccountName" "aster" "PersonaName" "Étoile" }
            "76561197960265738" { "AccountName" "Sol" "PersonaName" "sol" }
        }"#
            ),
        )?;
        fs::write(
            configs.join("config.vdf"),
            r#""InstallConfigStore" { "Software" { "Valve" { "Steam" { "Accounts" {
            "stale" { "SteamID" "76561197960265735" }
            "comet" { "SteamID" "76561197960265737" }
        } } } } }"#,
        )?;
        fs::write(
            steam.join("userdata/9/config/localconfig.vdf"),
            r#""UserLocalConfigStore" {
            "friends" { "123" { "PersonaName" "A Friend" } "PersonaName" "Stargazer" }
        }"#,
        )?;
        fs::write(steam.join("userdata/13/config/localconfig.vdf"), "broken")?;
        let list = || collect_profiles(vec![steamlocate::SteamDir::from_dir(steam)?]);
        let first = list()?;
        let id = |n| {
            format!(
                "{n}-{}",
                &digest(steam.canonicalize().unwrap().to_string_lossy().as_bytes())[..12]
            )
        };
        for (n, expected) in [
            (7, "aster (Étoile)"),
            (9, "comet (Stargazer)"),
            (10, "Sol"),
            (13, "Steam account 13"),
        ] {
            assert_eq!(
                first.iter().find(|p| p.id == id(n)).unwrap().label,
                expected
            );
        }
        fs::write(configs.join("loginusers.vdf"), "broken")?;
        let second = list()?;
        assert_eq!(
            second.iter().find(|p| p.id == id(7)).unwrap().label,
            "stale"
        );
        Ok(())
    }

    #[test]
    fn duplicate_personas_from_forgotten_logins_are_distinguishable() -> Result<()> {
        let fixture = tempfile::tempdir()?;
        let steam = fixture.path();
        for id in [23, 24] {
            let dir = steam.join(format!("userdata/{id}/config"));
            fs::create_dir_all(&dir)?;
            fs::write(
                dir.join("localconfig.vdf"),
                r#""UserLocalConfigStore" { "friends" { "PersonaName" "Nova" } }"#,
            )?;
        }
        let profiles = collect_profiles(vec![steamlocate::SteamDir::from_dir(steam)?])?;
        assert_eq!(profiles.len(), 2);
        assert_ne!(profiles[0].label, profiles[1].label);
        assert!(profiles.iter().all(|p| p.label.starts_with("Nova")));
        Ok(())
    }
}
