use crate::session::Interface;
use crate::{
    files::{Binding, ReviewedBinding},
    local::{Api, Local, Setup, discover, location, roots},
    model::*,
};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
};
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
// A review belongs to one account, game, server recipe revision and device setup.
// Save rechecks this snapshot under the same game lock used by play and enrollment.
#[derive(Clone)]
pub struct ReviewBase {
    game: String,
    identity: String,
    expected_revision: i64,
}

impl ReviewBase {
    pub fn new(
        local: &Local,
        c: &Credentials,
        game: &str,
        setup: Option<&Setup>,
        named_roots: &BTreeMap<String, PathBuf>,
        own_revision: Option<i64>,
    ) -> Result<Self> {
        let state = setup.map(|s| (&s.recipe, s.revision, s.shared, &s.bindings, &s.draft));
        let identity = digest(&serde_json::to_vec(&(
            local.scope(c),
            &c.device,
            game,
            state,
            named_roots,
        ))?);
        Ok(Self {
            game: game.to_owned(),
            identity,
            expected_revision: own_revision.unwrap_or_else(|| setup.map_or(0, |s| s.revision)),
        })
    }
}

pub struct ReviewedSetup {
    pub ignores: BTreeMap<String, BTreeSet<String>>,
    pub title: String,
    pub approved: Vec<(Selection, ReviewedBinding)>,
    pub removed: Vec<String>,
    pub shared_recipe: Option<String>,
    pub root_changes: BTreeMap<String, PathBuf>,
}

// Device-only edits become active together after the reviewed recipe is published.
#[derive(Clone, Serialize, Deserialize, Default)]
struct DeviceEdits {
    removed: BTreeSet<String>,
    roots: BTreeMap<String, (Option<PathBuf>, Binding)>,
}

impl DeviceEdits {
    fn checked_roots(
        &self,
        mut current: BTreeMap<String, PathBuf>,
    ) -> Result<BTreeMap<String, PathBuf>> {
        for (name, (before, root)) in &self.roots {
            ensure!(
                current.get(name) == before.as_ref() || current.get(name) == Some(&root.path),
                "root {name} changed since review; reopen setup"
            );
            Binding::for_selection(&root.path, &Kind::Folder, Some(root))?;
            current.insert(name.clone(), root.path.clone());
        }
        Ok(current)
    }
}

fn validate_review(
    recipe: &Recipe,
    existing: &BTreeMap<String, Binding>,
    draft: &Draft,
    edits: &DeviceEdits,
    home: &std::path::Path,
) -> Result<()> {
    recipe.validate()?;
    let mut bindings = existing.clone();
    bindings.retain(|name, _| recipe.slots.contains_key(name) && !edits.removed.contains(name));
    bindings.extend(draft.bindings.clone());
    if !bindings.is_empty() {
        let mut device_recipe = recipe.clone();
        device_recipe
            .slots
            .retain(|name, _| bindings.contains_key(name));
        ensure!(
            device_recipe.slots.len() == bindings.len(),
            "selected slot is missing from the recipe"
        );
        for (name, binding) in &draft.bindings {
            Binding::for_selection(
                &binding.path,
                &device_recipe.slots[name].kind,
                Some(binding),
            )?;
        }
        crate::files::Files::open(&device_recipe, &bindings, home)?;
    } else {
        ensure!(!existing.is_empty(), "choose at least one location to sync");
    }
    Ok(())
}

pub fn save_review(local: &Local, base: &ReviewBase, edits: ReviewedSetup) -> Result<()> {
    ensure!(key(&base.game), "invalid game ID");
    let c = local.credentials()?;
    let _lock = local.lock(&format!("{}:{}", local.scope(&c), base.game))?;
    let prior = local.maybe_setup(&c, &base.game)?;
    let named = local.named_roots(&c, &base.game)?;
    let current = ReviewBase::new(local, &c, &base.game, prior.as_ref(), &named, None)?;
    ensure!(
        current.identity == base.identity,
        "account or setup changed since review; reopen setup"
    );
    ensure!(
        prior.as_ref().is_none_or(|s| s.draft.is_none()),
        "apply or discard pending setup edits first"
    );

    let api = Api::new(c.clone())?;
    let (remote, online) = match api.rpc::<Vec<GameInfo>>(Action::Games) {
        Ok(list) => {
            let own = list.into_iter().find(|i| i.recipe.game == base.game);
            ensure!(
                own.as_ref().map_or(0, |g| g.revision) == base.expected_revision,
                "server recipe changed since review; reopen setup"
            );
            let info = match (own, edits.shared_recipe.as_deref()) {
                (Some(info), None) => info,
                (Some(_), Some(_)) => bail!("this account already has a recipe; reopen setup"),
                (None, hash) => {
                    let recipe = if let Some(hash) = hash {
                        let catalog: Vec<GameInfo> = api.rpc(Action::Catalog)?;
                        catalog
                            .into_iter()
                            .find(|g| {
                                g.recipe.game == base.game
                                    && recipe_hash(&g.recipe).is_ok_and(|h| h == hash)
                            })
                            .context("shared recipe changed since review; reopen setup")?
                            .recipe
                    } else {
                        Recipe {
                            game: base.game.clone(),
                            title: edits.title,
                            retention: 3,
                            slots: BTreeMap::new(),
                        }
                    };
                    GameInfo {
                        recipe,
                        revision: 0,
                        retired_slots: Default::default(),
                        shared: false,
                    }
                }
            };
            (info, true)
        }
        Err(e) if crate::local::unreachable(&e) => {
            let approved = prior
                .as_ref()
                .context("connect to enroll this game for the first time")?;
            ensure!(
                edits.shared_recipe.is_none() && approved.revision == base.expected_revision,
                "reconnect to review the latest recipe"
            );
            (info(approved), false)
        }
        Err(e) => return Err(e),
    };

    let mut s = prior.clone().unwrap_or_else(|| Setup::new(remote.clone()));
    let mut changes = DeviceEdits::default();
    for name in edits.removed {
        ensure!(
            s.bindings.contains_key(&name) && changes.removed.insert(name.clone()),
            "invalid selection to remove: {name}"
        );
    }
    let mut available = roots(&base.game);
    available.extend(named.clone());
    for (name, path) in edits.root_changes {
        crate::local::validate_root_name(&name)?;
        let binding = Binding::new(&path, &Kind::Folder)?;
        available.insert(name.clone(), binding.path.clone());
        changes
            .roots
            .insert(name.clone(), (named.get(&name).cloned(), binding));
    }

    // Reject invalid selections before creating any missing destinations.
    let mut selected: Vec<PathBuf> = s
        .bindings
        .iter()
        .filter(|(name, _)| {
            remote.recipe.slots.contains_key(*name)
                && !changes.removed.contains(*name)
                && !edits.approved.iter().any(|(sel, _)| &sel.slot == *name)
        })
        .map(|(_, binding)| binding.path.clone())
        .collect();
    let mut names = BTreeSet::new();
    let mut approved = edits.approved;
    for (sel, reviewed) in &mut approved {
        ensure!(
            sel.game == base.game
                && key(&sel.slot)
                && !changes.removed.contains(&sel.slot)
                && names.insert(sel.slot.clone()),
            "invalid or duplicate selection: {}",
            sel.slot
        );
        ensure!(
            !remote.retired_slots.contains(&sel.slot),
            "slot {} was removed; choose a new slot ID",
            sel.slot
        );
        let slot = remote.recipe.slots.get(&sel.slot);
        ensure!(
            slot.is_none_or(|slot| slot.kind == sel.kind),
            "the server expects a different file type for {}",
            sel.slot
        );
        let path = match &*reviewed {
            ReviewedBinding::Existing(binding) => {
                reviewed.validate(&sel.path, &sel.kind)?;
                binding.path.clone()
            }
            ReviewedBinding::Missing { base, relative, .. } => {
                ensure!(
                    !s.bindings.contains_key(&sel.slot),
                    "previously approved location disappeared; reconnect it or choose another"
                );
                let path = base.path.join(relative);
                ensure!(
                    slot.is_some_and(|slot| slot.locations.values().any(|loc| {
                        available.get(&loc.root).is_some_and(|root| {
                            root.canonicalize().ok().as_ref() == Some(&base.path)
                                && std::path::Path::new(&loc.relative) == relative
                                && (root.join(&loc.relative) == sel.path || path == sel.path)
                        })
                    })),
                    "destination is not the reviewed recipe location"
                );
                reviewed.validate(&path, &sel.kind)?;
                path
            }
        };
        ensure!(
            !path.starts_with(&local.home) && !local.home.starts_with(&path),
            "save selection overlaps Buddy's own data"
        );
        ensure!(
            !selected
                .iter()
                .any(|p| p.starts_with(&path) || path.starts_with(p)),
            "overlapping selections"
        );
        selected.push(path.clone());
        sel.path = path;
    }
    let mut draft = Draft::new(remote.clone());
    let platform = runtime(&base.game);
    let mut created = BTreeMap::new();
    for (sel, reviewed) in approved {
        let binding = reviewed.resolve_with_created(&sel.path, &sel.kind, &mut created)?;
        draft.bind(sel, binding, &platform, &available)?;
    }
    for (name, rules) in edits.ignores {
        draft
            .desired
            .recipe
            .slots
            .get_mut(&name)
            .context("ignore location is missing; reopen setup")?
            .set_ignores(rules)?;
    }
    validate_review(
        &draft.desired.recipe,
        &s.bindings,
        &draft,
        &changes,
        &local.home,
    )?;
    draft.device_edits = Some(changes);
    s.draft = Some(draft);
    local.save(&c, &s)?;
    if online
        && idle(&s)
        && let Err(e) = apply(local, &api, &mut s, &remote, Interface::Unattended)
    {
        match e.downcast_ref::<crate::local::ApiError>().map(|e| e.0) {
            // A lease or competing edit won the compare-and-swap. The next attempt
            // reads the current revision and reconciles the whole draft before retrying.
            Some(412) => return Ok(()),
            Some(400..=499) => {
                if let Some(before) = &prior {
                    local.save(&c, before)?;
                } else {
                    local.forget_provisional_setup(&c, &base.game)?;
                }
            }
            _ => {}
        }
        return Err(e);
    }
    Ok(())
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
                        retired_slots: Default::default(),
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
    ensure!(
        !info.retired_slots.contains(&sel.slot),
        "slot {} was removed from this recipe; choose a new --slot ID",
        sel.slot
    );
    let mut draft = s.draft.clone().unwrap_or_else(|| Draft::new(info.clone()));
    let platform = runtime(&sel.game);
    let mut available = roots(&sel.game);
    available.extend(local.named_roots(&c, &sel.game)?);
    let binding = Binding::for_selection(&sel.path, &sel.kind, reviewed)?;
    draft.bind(sel, binding, &platform, &available)?;
    draft.desired.recipe.validate()?;
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    device_edits: Option<DeviceEdits>,
}
impl Draft {
    fn bind(
        &mut self,
        sel: Selection,
        binding: Binding,
        platform: &str,
        available: &BTreeMap<String, PathBuf>,
    ) -> Result<()> {
        let slot = self
            .desired
            .recipe
            .slots
            .entry(sel.slot.clone())
            .or_insert_with(|| Slot {
                kind: sel.kind.clone(),
                codec: raw_codec(),
                ignores: Default::default(),
                settings: sel.settings,
                source: sel.source,
                locations: BTreeMap::new(),
            });
        ensure!(
            slot.kind == sel.kind,
            "the server expects a different file type for {}",
            sel.slot
        );
        if !slot.locations.contains_key(platform)
            && let Some(loc) = location(&std::path::absolute(&sel.path)?, available)
                .or_else(|| location(&binding.path, available))
        {
            slot.locations.insert(platform.into(), loc);
        }
        if let Some(edits) = &mut self.device_edits {
            edits.removed.remove(&sel.slot);
        }
        self.bindings.insert(sel.slot, binding);
        Ok(())
    }
    fn new(info: GameInfo) -> Self {
        Self {
            base: info.clone(),
            desired: info,
            bindings: BTreeMap::new(),
            device_edits: None,
        }
    }
}
fn info(s: &Setup) -> GameInfo {
    GameInfo {
        recipe: s.recipe.clone(),
        revision: s.revision,
        shared: s.shared,
        retired_slots: s.retired_slots.clone(),
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
        ensure!(
            !remote.retired_slots.contains(name),
            "slot {name} was removed; discard this draft and review the remaining locations"
        );
        let merged = if let Some(r) = current
            && r.kind == slot.kind
        {
            let mut merged = r.clone();
            let base_rules = prior.map(|s| s.ignores.clone()).unwrap_or_default();
            let mut rules = r.ignores.clone();
            for rule in base_rules.difference(&slot.ignores) {
                rules.remove(rule);
            }
            rules.extend(slot.ignores.difference(&base_rules).cloned());
            merged.set_ignores(rules)?;
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
            !remote.retired_slots.contains(name),
            "slot {name} was removed; discard this draft and review the remaining locations"
        );
        ensure!(
            next.slots.get(name).map(|s| &s.kind) == ours.slots.get(name).map(|s| &s.kind),
            "selection {name} changed type; discard the draft and bind it again"
        );
    }
    let checked_roots = if let Some(edits) = &d.device_edits {
        validate_review(&next, &s.bindings, d, edits, &local.home)?;
        Some(edits.checked_roots(local.named_roots(&api.c, &next.game)?)?)
    } else {
        None
    };
    let rev = if remote.revision == 0 || remote.recipe != next || remote.shared != shared {
        api.rpc(Action::SaveRecipe {
            recipe: next.clone(),
            expected: remote.revision,
            shared,
        })?
    } else {
        remote.revision
    };
    if let Some(edits) = &d.device_edits {
        if !edits.removed.is_empty() {
            s.offline_prep = None;
        }
        s.bindings.retain(|name, _| !edits.removed.contains(name));
        if s.bindings.is_empty() && d.bindings.is_empty() {
            s.bindings_cleared = true;
            s.approved_proton_prefix = None;
            s.needs_first_pull = false;
        }
    }
    for (name, b) in &d.bindings {
        if s.bindings.get(name).is_some_and(|old| old != b) {
            s.base = None;
        }
        if s.bindings.get(name) != Some(b) {
            s.offline_prep = None;
        }
        s.bindings.insert(name.clone(), b.clone());
    }
    s.draft = None;
    accept_recipe(
        s,
        GameInfo {
            recipe: next,
            shared,
            revision: rev,
            retired_slots: remote.retired_slots.clone(),
        },
    );
    if let Some(roots) = checked_roots {
        local.save_setup_with_roots(&api.c, s, &roots)
    } else {
        local.save(&api.c, s)
    }
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
        if s.revision == 0
            && s.draft.is_some()
            && s.bindings.is_empty()
            && s.base.is_none()
            && idle(&s)
        {
            // A first enrollment may have stopped after staging, before any approval
            // became active. Discard only the local draft; a remote recipe stays discoverable.
            return local.forget_provisional_setup(&c, game);
        }
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
// Callers with pending sessions archive changes to retired slots before adopting
// the recipe. Each remaining session keeps its own comparison base and order.
pub fn accept_recipe(s: &mut Setup, info: GameInfo) {
    let prior_count = s.bindings.len();
    s.bindings
        .retain(|name, _| info.recipe.slots.contains_key(name));
    if prior_count > 0 && s.bindings.is_empty() {
        s.bindings_cleared = true;
        s.approved_proton_prefix = None;
        s.needs_first_pull = false;
    }
    let changed_scope = prior_count != s.bindings.len()
        || s.bindings.keys().any(|name| {
            s.recipe.slots.get(name).map(|slot| &slot.ignores)
                != info.recipe.slots.get(name).map(|slot| &slot.ignores)
        });
    if s.bindings.is_empty()
        || (changed_scope
            && matches!(
                s.offline_prep,
                Some(crate::local::OfflinePrep::Ready { .. })
            ))
    {
        s.offline_prep = None;
    }
    s.recipe = info.recipe;
    s.revision = info.revision;
    s.shared = info.shared;
    s.retired_slots = info.retired_slots;
    rescope(s);
}
pub fn set_ignore(
    local: &Local,
    game: &str,
    path: &str,
    ignore: bool,
    reviewed: Option<(&str, i64)>,
) -> Result<()> {
    let c = local.credentials()?;
    let scope = local.scope(&c);
    ensure!(
        reviewed.is_none_or(|(account, _)| account == scope),
        "Account changed. Reopen this game."
    );
    let _lock = local.lock(&format!("{scope}:{game}"))?;
    let mut s = local.setup(&c, game)?;
    ensure!(
        idle(&s) && s.draft.is_none(),
        "Finish pending sync and recipe edits before changing ignores."
    );
    let api = Api::new(c.clone())?;
    let mut remote = api
        .rpc::<Vec<GameInfo>>(Action::Games)?
        .into_iter()
        .find(|g| g.recipe.game == game)
        .context("Recipe unavailable. Refresh the library.")?;
    ensure!(
        reviewed.is_none_or(|(_, rev)| rev == remote.revision),
        "Recipe changed. Reopen the game to review it."
    );
    let (name, relative) = logical_path(path);
    let slot = remote
        .recipe
        .slots
        .get_mut(name)
        .context("Location was removed. Reopen the recipe.")?;
    if slot.edit_ignore(relative, ignore)? {
        remote.recipe.validate()?;
        remote.revision = api.rpc(Action::SaveRecipe { recipe: remote.recipe.clone(), expected: remote.revision, shared: remote.shared })
            .context("Couldn’t update the recipe. Close active games and check that the server supports ignores.")?;
    }
    accept_recipe(&mut s, remote);
    local.save(&c, &s)
}
pub fn remove_slot(local: &Local, base: &ReviewBase, slot: &str) -> Result<()> {
    let c = local.credentials()?;
    let _lock = local.lock(&format!("{}:{}", local.scope(&c), base.game))?;
    let mut setup = local.maybe_setup(&c, &base.game)?;
    let named = local.named_roots(&c, &base.game)?;
    let current = ReviewBase::new(local, &c, &base.game, setup.as_ref(), &named, None)?;
    ensure!(
        current.identity == base.identity,
        "account or setup changed since review; reopen setup"
    );
    ensure!(
        setup.as_ref().is_none_or(|s| idle(s) && s.draft.is_none()),
        "finish pending sessions and setup edits before removing a recipe location"
    );
    let remote = Api::new(c.clone())?.rpc(Action::RemoveSlot {
        game: base.game.clone(),
        slot: slot.to_owned(),
        expected: base.expected_revision,
    })?;
    if let Some(s) = &mut setup {
        accept_recipe(s, remote);
        local.save(&c, s)?;
    }
    Ok(())
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
    s.offline_prep = None;
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
                    retired_slots: Default::default(),
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
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum DeviceState {
    New,
    NeedsLocations,
    Pending,
    Ready,
    Paused,
}
impl DeviceState {
    pub fn from_setup(s: Option<&Setup>) -> Self {
        match s {
            None => Self::New,
            Some(s) if !s.bindings.is_empty() => Self::Ready,
            Some(s) if s.draft.is_some() => Self::Pending,
            Some(s) if s.bindings_cleared => Self::Paused,
            Some(_) => Self::NeedsLocations,
        }
    }
    pub fn needs_locations(self) -> bool {
        matches!(self, Self::New | Self::NeedsLocations)
    }
}
#[derive(Clone)]
pub struct Row {
    pub id: String,
    pub title: String,
    pub installed: bool,
    pub mine: bool,
    pub shared: bool,
    pub device: DeviceState,
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
                device: DeviceState::New,
            },
        );
    }
    for s in local.setups(&c)? {
        let device = DeviceState::from_setup(Some(&s));
        let cached = s.revision > 0;
        let r = rows.entry(s.recipe.game.clone()).or_insert(Row {
            id: s.recipe.game.clone(),
            title: s.recipe.title,
            installed: false,
            mine: false,
            shared: false,
            device,
        });
        r.device = device;
        r.mine = cached;
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
                device: DeviceState::New,
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
pub fn inferred_game(g: Option<String>) -> Result<String> {
    g.or_else(|| {
        std::env::var("SteamAppId")
            .ok()
            .map(|id| format!("steam-{id}"))
    })
    .context("pass --game for a non-Steam launch")
}

#[cfg(test)]
mod tests;
