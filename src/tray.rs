use crate::{
    markdown, ops, text, ui_text,
    vault::{Profile, Vault},
    work,
};
use chrono::Local;
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};
use tezbar_extension_sdk::{self as sdk, CoreResult, ExtensionContext};
use tokio::sync::{Mutex, Notify};

pub const RESOURCE: &str = "obsidian-vaults";

#[derive(Clone)]
enum Action {
    Vault(Vault),
    Daily(Vault),
    Bookmark(Vault, String, crate::fs::Identity),
    Refresh,
    Hide,
}

#[derive(Default)]
struct State {
    active: bool,
    worker: bool,
    prefs: Value,
    actions: HashMap<String, Action>,
}
#[derive(Default)]
pub struct Tray {
    state: Mutex<State>,
    change: Notify,
    sequence: AtomicU64,
}

fn pref_subset(prefs: &Value) -> Value {
    let mut subset = json!({});
    for key in ["vaultPath", "configFileName"] {
        let value = text(prefs, key);
        if !value.is_empty() && value.len() <= 4096 {
            subset[key] = json!(value);
        }
    }
    subset
}

struct Builder {
    revision: u64,
    index: usize,
    actions: HashMap<String, Action>,
}
impl Builder {
    fn item(&mut self, title: &str, action: Option<Action>, children: Vec<Value>) -> Value {
        self.index += 1;
        let id = format!("r{}-{}", self.revision, self.index);
        if let Some(action) = action {
            self.actions.insert(id.clone(), action);
        }
        let mut item = json!({"id":id,"title":ui_text(title,160)});
        if !children.is_empty() {
            item["kind"] = json!("submenu");
            item["items"] = json!(children);
        }
        item
    }
}

impl Tray {
    async fn refresh(
        &self,
        ctx: &ExtensionContext,
        profile: &Profile,
        windows: bool,
        prefs: &Value,
    ) -> Result<HashMap<String, Action>, String> {
        let source = profile.clone();
        let prefs = prefs.clone();
        let (vaults, bookmarks) = work(ctx, move |cancel| {
            let vaults = source.discover(windows, &prefs)?;
            let mut entries = Vec::new();
            for vault in vaults.iter().take(8) {
                if cancel.load(Ordering::Relaxed) {
                    return Err("Operation cancelled".into());
                }
                let root = match vault.open() {
                    Ok(root) => root,
                    Err(_) => {
                        entries.push(vec![]);
                        continue;
                    }
                };
                let mut paths = vault
                    .bookmarks()
                    .unwrap_or_default()
                    .into_iter()
                    .collect::<Vec<_>>();
                paths.sort();
                entries.push(
                    paths
                        .into_iter()
                        .filter_map(|path| {
                            root.stamp(&path).ok().map(|stamp| (path, stamp.identity))
                        })
                        .take(9)
                        .collect::<Vec<_>>(),
                );
            }
            Ok((vaults, entries))
        })
        .await?;
        let mut builder = Builder {
            index: 0,
            revision: self.sequence.fetch_add(1, Ordering::Relaxed),
            actions: HashMap::new(),
        };
        let mut vault_items = vec![];
        let mut daily_items = vec![];
        let mut bookmark_items = vec![];
        for (index, vault) in vaults.iter().take(8).enumerate() {
            let label = if vaults
                .iter()
                .filter(|other| other.name == vault.name)
                .count()
                > 1
            {
                vault.display()
            } else {
                vault.name.clone()
            };
            vault_items.push(builder.item(&label, Some(Action::Vault(vault.clone())), vec![]));
            if vault.plugin("daily-notes").unwrap_or(false) {
                daily_items.push(builder.item(&label, Some(Action::Daily(vault.clone())), vec![]));
            }
            let mut children = vec![];
            for (path, identity) in &bookmarks[index] {
                children.push(builder.item(
                    path,
                    Some(Action::Bookmark(vault.clone(), path.clone(), *identity)),
                    vec![],
                ));
            }
            if !children.is_empty() {
                bookmark_items.push(builder.item(&label, None, children));
            }
        }
        let mut items = vec![];
        if !vault_items.is_empty() {
            items.push(builder.item("Open Vault", None, vault_items));
        }
        if !daily_items.is_empty() {
            items.push(builder.item("Daily Note", None, daily_items));
        }
        if !bookmark_items.is_empty() {
            items.push(builder.item("Bookmarked Notes", None, bookmark_items));
        }
        if items.is_empty() {
            let mut status = builder.item("No configured Obsidian vaults", None, vec![]);
            status["kind"] = json!("section");
            items.push(status);
        }
        items.push(builder.item("Refresh Obsidian Menu", Some(Action::Refresh), vec![]));
        items.push(builder.item("Hide Obsidian Menu", Some(Action::Hide), vec![]));
        ctx.host("host.tray.set",json!({"resourceId":RESOURCE,"title":"Obsidian","tooltip":"Obsidian Vaults","icon":"resources/extension_icon.png","items":items})).await?;
        Ok(builder.actions)
    }

    pub async fn enable(
        self: &Arc<Self>,
        ctx: &ExtensionContext,
        root: &ExtensionContext,
        profile: &Profile,
        windows: bool,
        prefs: &Value,
    ) -> CoreResult {
        let mut state = self.state.lock().await;
        let retained = pref_subset(prefs);
        if !state.worker {
            let worker = root.clone();
            let owner = self.clone();
            let profile = profile.clone();
            root.spawn(async move {
                loop {
                    if !owner.state.lock().await.active{owner.change.notified().await;continue;}
                    tokio::select! {
                        _=owner.change.notified()=>{},
                        _=tokio::time::sleep(Duration::from_secs(300))=>{
                            let mut state=owner.state.lock().await;
                            if state.active {
                                if let Ok(actions)=owner.refresh(&worker,&profile,windows,&state.prefs).await{state.actions=actions;}
                            }
                        }
                    }
                }
            })?;
            state.worker = true;
        }
        ctx.host(
            "host.background.retain",
            json!({"key":RESOURCE,"durable":true,"data":retained}),
        )
        .await?;
        match self.refresh(ctx, profile, windows, &retained).await {
            Ok(actions) => {
                state.actions = actions;
                state.prefs = retained;
                state.active = true;
                self.change.notify_one();
                Ok(json!({"ready":true}))
            }
            Err(error) => {
                if state.active {
                    let _ = root
                        .host(
                            "host.background.retain",
                            json!({"key":RESOURCE,"durable":true,"data":state.prefs}),
                        )
                        .await;
                } else {
                    let _ = root
                        .host("host.background.release", json!({"key":RESOURCE}))
                        .await;
                }
                Err(error)
            }
        }
    }

    pub async fn event(
        &self,
        ctx: &ExtensionContext,
        profile: &Profile,
        windows: bool,
        data: &Value,
    ) -> CoreResult {
        if data["resourceId"] != RESOURCE {
            return Err("Unknown Obsidian status resource".into());
        }
        let mut state = self.state.lock().await;
        if !state.active {
            return Err("Obsidian status item has been disabled".into());
        }
        let action = state
            .actions
            .get(text(data, "actionId"))
            .cloned()
            .ok_or("Stale Obsidian menu action")?;
        match action {
            Action::Refresh => {
                state.actions = self.refresh(ctx, profile, windows, &state.prefs).await?;
            }
            Action::Hide => {
                ctx.host("host.background.release", json!({"key":RESOURCE}))
                    .await?;
                state.active = false;
                state.actions.clear();
                self.change.notify_one();
                ctx.host("host.tray.remove", json!({"resourceId":RESOURCE}))
                    .await?;
            }
            Action::Vault(vault) => {
                vault.open()?;
                sdk::open(ctx, &ops::vault_uri(&vault)).await?;
            }
            Action::Daily(vault) => {
                let (path, seed) = markdown::daily_target(&vault, Local::now())?;
                let root = vault.open()?;
                match root.stamp(&path) {
                    Ok(_) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => root
                        .create(&path, seed.as_bytes())
                        .map_err(|error| error.to_string())?,
                    Err(error) => return Err(error.to_string()),
                }
                sdk::open(ctx, &ops::note_uri(&vault, &path)?).await?;
            }
            Action::Bookmark(vault, path, identity) => {
                if vault
                    .open()?
                    .stamp(&path)
                    .map_err(|e| e.to_string())?
                    .identity
                    != identity
                    || !vault.bookmarks()?.contains(&path)
                {
                    return Err("Bookmark changed. Refresh the menu.".into());
                }
                sdk::open(ctx, &ops::note_uri(&vault, &path)?).await?;
            }
        }
        Ok(json!({"ok":true}))
    }
}
