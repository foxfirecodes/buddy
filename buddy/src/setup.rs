use crate::session::Interface;
use crate::{
    files::Binding,
    local::{Api, Local, Setup, discover, location, roots},
    model::*,
};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::PathBuf};
pub fn runtime(game: &str) -> String {
    if cfg!(target_os = "linux") && roots(game).contains_key("pfx") {
        return "proton".into();
    }
    std::env::consts::OS.into()
}
pub struct Selection {
    pub game: String,
    pub title: String,
    pub slot: String,
    pub path: PathBuf,
    pub kind: Kind,
    pub settings: bool,
    pub source: Option<String>,
}
pub fn bind(local: &Local, sel: Selection, reviewed: Option<&Binding>) -> Result<()> {
    ensure!(
        key(&sel.game) && key(&sel.slot),
        "use letters, numbers, - or _ in IDs"
    );
    let c = local.credentials()?;
    let _lock = local.lock(&format!("{}:{}", local.scope(&c), sel.game))?;
    let api = Api::new(c.clone())?;
    let setup = local
        .setups(&c)?
        .into_iter()
        .find(|s| s.recipe.game == sel.game);
    let remote = api.rpc::<Vec<GameInfo>>(Action::Games);
    let (info, online) = match remote {
        Ok(games) => {
            if let Some(own) = games.into_iter().find(|i| i.recipe.game == sel.game) {
                (own, true)
            } else {
                let catalog: Vec<GameInfo> = api.rpc(Action::Catalog)?;
                let mut matches = catalog.into_iter().filter(|i| i.recipe.game == sel.game);
                let shared = matches.next();
                ensure!(
                    matches.next().is_none(),
                    "several shared recipes exist; import the one you want first"
                );
                let recipe = shared.map(|i| i.recipe).unwrap_or_else(|| Recipe {
                    game: sel.game.clone(),
                    title: sel.title.clone(),
                    slots: BTreeMap::new(),
                    retention: 3,
                });
                (
                    GameInfo {
                        recipe,
                        shared: false,
                        revision: 0,
                    },
                    true,
                )
            }
        }
        Err(e) if crate::local::unreachable(&e) => {
            let Some(s) = &setup else {
                return Err(e.context("connect to enroll this game for the first time"));
            };
            (info(s), false)
        }
        Err(e) => return Err(e),
    };
    let mut s = setup.unwrap_or_else(|| Setup::new(info.clone()));
    let mut draft = s.draft.clone().unwrap_or_else(|| Draft::new(info.clone()));
    let binding = Binding::for_selection(&sel.path, &sel.kind, reviewed)?;
    let mut recipe = draft.desired.recipe.clone();
    let platform = runtime(&sel.game);
    let mut available = roots(&sel.game);
    available.extend(local.named_roots(&c, &sel.game)?);
    let loc = location(&std::path::absolute(&sel.path)?, &available)
        .or_else(|| location(&binding.path, &available));
    let slot = recipe.slots.entry(sel.slot.clone()).or_insert(Slot {
        kind: sel.kind.clone(),
        codec: raw_codec(),
        settings: sel.settings,
        source: sel.source,
        locations: BTreeMap::new(),
    });
    ensure!(
        slot.kind == sel.kind,
        "the server expects a different file type for this slot"
    );
    if !slot.locations.contains_key(&platform)
        && let Some(loc) = loc
    {
        slot.locations.insert(platform, loc);
    }
    recipe.validate()?;
    draft.desired.recipe = recipe;
    draft.bindings.insert(sel.slot, binding);
    s.draft = Some(draft);
    local.save(&c, &s)?;
    if online && idle(&s) {
        apply(local, &api, &mut s, &info, Interface::Unattended)?;
    }
    Ok(())
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Draft {
    base: GameInfo,
    pub desired: GameInfo,
    bindings: BTreeMap<String, Binding>,
}
impl Draft {
    fn new(info: GameInfo) -> Self {
        Self {
            base: info.clone(),
            desired: info,
            bindings: BTreeMap::new(),
        }
    }
}
fn info(s: &Setup) -> GameInfo {
    GameInfo {
        recipe: s.recipe.clone(),
        revision: s.revision,
        shared: s.shared,
    }
}
pub fn idle(s: &Setup) -> bool {
    !s.active && s.pending.is_empty() && s.restore.is_none()
}
fn field<T: Clone + PartialEq + std::fmt::Debug>(
    name: &str,
    base: &T,
    ours: &T,
    theirs: &T,
    ui: Interface,
) -> Result<T> {
    if ours == base || ours == theirs {
        return Ok(theirs.clone());
    }
    if theirs == base {
        return Ok(ours.clone());
    }
    let msg =
        format!("Both devices edited {name}.\n\nYour edit: {ours:?}\nServer value: {theirs:?}");
    match ui.choose(
        "Choose a recipe setting",
        &msg,
        &["Use my edit", "Keep server value", "Decide later"],
    )? {
        0 => Ok(ours.clone()),
        1 => Ok(theirs.clone()),
        _ => bail!("recipe draft kept on this device"),
    }
}
fn apply(local: &Local, api: &Api, s: &mut Setup, remote: &GameInfo, ui: Interface) -> Result<()> {
    let d = s.draft.as_ref().context("no pending recipe edits")?;
    let (base, ours, theirs) = (&d.base.recipe, &d.desired.recipe, &remote.recipe);
    let mut next = theirs.clone();
    next.title = field("game title", &base.title, &ours.title, &theirs.title, ui)?;
    next.retention = field(
        "backup retention",
        &base.retention,
        &ours.retention,
        &theirs.retention,
        ui,
    )?;
    let shared = field(
        "recipe sharing",
        &d.base.shared,
        &d.desired.shared,
        &remote.shared,
        ui,
    )?;
    for (name, slot) in &ours.slots {
        let prior = base.slots.get(name);
        let current = theirs.slots.get(name);
        if prior == Some(slot) {
            continue;
        }
        let merged = if let Some(r) = current
            && r.kind == slot.kind
        {
            let mut merged = r.clone();
            merged.settings = field(
                &format!("{name} settings tag"),
                &prior.is_some_and(|s| s.settings),
                &slot.settings,
                &r.settings,
                ui,
            )?;
            merged.source = field(
                &format!("{name} source"),
                &prior.and_then(|s| s.source.clone()),
                &slot.source,
                &r.source,
                ui,
            )?;
            for (platform, loc) in &slot.locations {
                let loc = field(
                    &format!("{name} on {platform}"),
                    &prior.and_then(|s| s.locations.get(platform)).cloned(),
                    &Some(loc.clone()),
                    &r.locations.get(platform).cloned(),
                    ui,
                )?;
                if let Some(loc) = loc {
                    merged.locations.insert(platform.clone(), loc);
                }
            }
            merged
        } else {
            field(
                name,
                &prior.cloned(),
                &Some(slot.clone()),
                &current.cloned(),
                ui,
            )?
            .context("selection removed")?
        };
        next.slots.insert(name.clone(), merged);
    }
    next.validate()?;
    for name in d.bindings.keys() {
        ensure!(
            next.slots.get(name).map(|s| &s.kind) == ours.slots.get(name).map(|s| &s.kind),
            "selection {name} changed type; discard the draft and bind it again"
        );
    }
    let rev = if remote.revision == 0 || remote.recipe != next || remote.shared != shared {
        api.rpc(Action::SaveRecipe {
            recipe: next.clone(),
            expected: remote.revision,
            shared,
        })?
    } else {
        remote.revision
    };
    for (name, b) in &d.bindings {
        if s.bindings.get(name).is_some_and(|old| old != b) {
            s.base = None;
        }
        s.bindings.insert(name.clone(), b.clone());
    }
    s.recipe = next;
    s.shared = shared;
    s.revision = rev;
    s.draft = None;
    rescope(s);
    local.save(&api.c, s)
}
pub fn publish_draft(local: &Local, api: &Api, s: &mut Setup, ui: Interface) -> Result<()> {
    if s.draft.is_none() {
        return Ok(());
    }
    ensure!(
        idle(s),
        "sync outstanding game sessions before applying recipe edits"
    );
    let list: Vec<GameInfo> = api.rpc(Action::Games)?;
    let remote = list
        .into_iter()
        .find(|i| i.recipe.game == s.recipe.game)
        .unwrap_or_else(|| s.draft.as_ref().unwrap().base.clone());
    apply(local, api, s, &remote, ui)
}
pub fn review(local: &Local, game: &str, ui: Interface, discard: bool) -> Result<()> {
    let c = local.credentials()?;
    let _lock = local.lock(&format!("{}:{game}", local.scope(&c)))?;
    let mut s = local.setup(&c, game)?;
    if discard {
        s.draft = None;
        local.save(&c, &s)
    } else {
        publish_draft(local, &Api::new(c)?, &mut s, ui)
    }
}
fn rescope(s: &mut Setup) {
    let mut selected = s.recipe.clone();
    selected
        .slots
        .retain(|name, _| s.bindings.contains_key(name));
    s.base = s.base.as_ref().map(|base| base.project(&selected));
}
pub fn unbind(local: &Local, game: &str, slot: &str) -> Result<()> {
    let c = local.credentials()?;
    let _lock = local.lock(&format!("{}:{game}", local.scope(&c)))?;
    let mut s = local.setup(&c, game)?;
    ensure!(
        !s.active && s.restore.is_none() && s.pending.is_empty(),
        "sync the previous session before changing its selections"
    );
    ensure!(
        s.draft.is_none(),
        "apply or discard pending recipe edits first"
    );
    ensure!(
        s.bindings.remove(slot).is_some(),
        "selection is not bound on this device"
    );
    if s.bindings.is_empty() {
        s.bindings_cleared = true;
        s.approved_proton_prefix = None;
        s.needs_first_pull = false;
    }
    rescope(&mut s);
    local.save(&c, &s)
}
pub fn policy(
    local: &Local,
    game: &str,
    retention: Option<u32>,
    shared: Option<bool>,
) -> Result<()> {
    let c = local.credentials()?;
    let _lock = local.lock(&format!("{}:{game}", local.scope(&c)))?;
    let mut s = local.setup(&c, game)?;
    let base = info(&s);
    let d = s.draft.get_or_insert_with(|| Draft::new(base));
    if let Some(v) = retention {
        d.desired.recipe.retention = v;
    }
    if let Some(v) = shared {
        d.desired.shared = v;
    }
    d.desired.recipe.validate()?;
    local.save(&c, &s)?;
    if idle(&s) {
        match publish_draft(local, &Api::new(c)?, &mut s, Interface::Unattended) {
            Err(e) if crate::local::unreachable(&e) => Ok(()),
            other => other,
        }
    } else {
        Ok(())
    }
}
pub fn import(local: &Local, game: &str, hash: &str) -> Result<()> {
    let c = local.credentials()?;
    let _lock = local.lock(&format!("{}:{game}", local.scope(&c)))?;
    let api = Api::new(c.clone())?;
    let catalog: Vec<GameInfo> = api.rpc(Action::Catalog)?;
    for info in catalog {
        if info.recipe.game == game && recipe_hash(&info.recipe)? == hash {
            let rev = api.rpc(Action::SaveRecipe {
                recipe: info.recipe.clone(),
                expected: 0,
                shared: false,
            })?;
            return local.save(
                &c,
                &Setup::new(GameInfo {
                    recipe: info.recipe,
                    revision: rev,
                    shared: false,
                }),
            );
        }
    }
    bail!("shared recipe unavailable")
}
pub fn recipe_hash(recipe: &Recipe) -> Result<String> {
    Ok(digest(&serde_json::to_vec(recipe)?))
}
#[derive(Clone)]
pub struct Row {
    pub id: String,
    pub title: String,
    pub installed: bool,
    pub mine: bool,
    pub shared: bool,
    pub local: bool,
}
pub fn games(local: &Local) -> Result<Vec<Row>> {
    let c = local.credentials()?;
    let api = Api::new(c.clone())?;
    let mut rows = BTreeMap::new();
    for (id, title, _) in discover() {
        rows.insert(
            id.clone(),
            Row {
                id,
                title,
                installed: true,
                mine: false,
                shared: false,
                local: false,
            },
        );
    }
    for s in local.setups(&c)? {
        let r = rows.entry(s.recipe.game.clone()).or_insert(Row {
            id: s.recipe.game.clone(),
            title: s.recipe.title,
            installed: false,
            mine: false,
            shared: false,
            local: true,
        });
        r.local = true;
    }
    for (action, mine) in [(Action::Games, true), (Action::Catalog, false)] {
        let list = match api.rpc::<Vec<GameInfo>>(action) {
            Ok(list) => list,
            Err(e) if crate::local::unreachable(&e) => break,
            Err(e) => return Err(e),
        };
        for info in list {
            let recipe = info.recipe;
            let r = rows.entry(recipe.game.clone()).or_insert(Row {
                id: recipe.game,
                title: recipe.title,
                installed: false,
                mine: false,
                local: false,
                shared: false,
            });
            if mine {
                r.mine = true;
            } else {
                r.shared = true;
            }
        }
    }
    let mut list = rows.into_values().collect::<Vec<_>>();
    list.sort_by_cached_key(|r| r.title.to_lowercase());
    Ok(list)
}
pub fn suggest(local: &Local, game: &str) -> Result<Vec<(String, Kind, Option<PathBuf>)>> {
    let api = Api::new(local.credentials()?)?;
    let list = match api.rpc::<Vec<GameInfo>>(Action::Games) {
        Ok(own) if own.iter().any(|i| i.recipe.game == game) => own,
        Ok(_) => api.rpc(Action::Catalog)?,
        Err(e) if crate::local::unreachable(&e) => local
            .setups(&api.c)?
            .into_iter()
            .map(|s| GameInfo {
                recipe: s.recipe,
                revision: s.revision,
                shared: s.shared,
            })
            .collect(),
        Err(e) => return Err(e),
    };
    let mut matches = list.into_iter().filter(|i| i.recipe.game == game);
    let Some(info) = matches.next() else {
        return Ok(vec![]);
    };
    if matches.next().is_some() {
        return Ok(vec![]);
    }
    let platform = runtime(game);
    let mut roots = roots(game);
    let named = local.named_roots(&api.c, game)?;
    roots.extend(named.clone());
    let mut result = vec![];
    for (name, slot) in info.recipe.slots {
        let path = slot
            .locations
            .get(&platform)
            .or_else(|| {
                slot.locations
                    .values()
                    .find(|l| named.contains_key(&l.root))
            })
            .and_then(|l| roots.get(&l.root).map(|p| p.join(&l.relative)));
        result.push((name, slot.kind, path));
    }
    Ok(result)
}
pub fn inferred_game(g: Option<String>) -> Result<String> {
    g.or_else(|| {
        std::env::var("SteamAppId")
            .ok()
            .map(|id| format!("steam-{id}"))
    })
    .context("pass --game for a non-Steam launch")
}
