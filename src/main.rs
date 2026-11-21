mod fs;
mod markdown;
mod model;
mod ops;
mod tray;
mod vault;
mod view;

use chrono::Local;
use markdown::Hit;
use model::{Callback, CustomAction, Form, NoteOp, Page, State};
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use tezbar_extension_sdk::{self as sdk, BoxFuture, CoreResult, Extension, ExtensionContext};
use vault::Profile;

const ID: &str = "com.tezbar.native.obsidian";
const COMMANDS: &[&str] = &[
    "searchNoteCommand",
    "starredNotesCommand",
    "openVaultCommand",
    "openWorkspaceCommand",
    "dailyNoteAppendCommand",
    "dailyNoteCommand",
    "createNoteCommand",
    "randomNoteCommand",
    "searchMedia",
    "obsidianMenuBar",
    "appendTaskCommand",
    "runActionCommand",
    "quickCaptureCommand",
];
const CUSTOM_STORE: &str = "custom-actions";

type Transition = Result<Option<Value>, String>;
fn reply(result: CoreResult) -> Transition {
    result.map(Some)
}
pub fn text<'a>(v: &'a Value, key: &str) -> &'a str {
    v[key].as_str().unwrap_or("")
}
pub fn ui_text(input: &str, max: usize) -> String {
    let mut out = String::new();
    for ch in input.chars().filter(|ch| !ch.is_control()) {
        if out.len() + ch.len_utf8() > max {
            break;
        }
        out.push(ch);
    }
    out
}

pub struct Port {
    profile: Profile,
    root: Mutex<Option<ExtensionContext>>,
    tray: Arc<tray::Tray>,
}

struct OnDrop(Arc<AtomicBool>);
impl Drop for OnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

pub async fn work<T: Send + 'static>(
    ctx: &ExtensionContext,
    f: impl FnOnce(&AtomicBool) -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    let stop = Arc::new(AtomicBool::new(false));
    let _cancel = OnDrop(stop.clone());
    let task = tokio::task::spawn_blocking(move || f(&stop));
    tokio::select! {biased; _=ctx.cancelled()=>Err("Operation cancelled".into()), result=task=>result.map_err(|_|"Obsidian file worker exited unexpectedly")?}
}

fn query_arg(command: &str, args: &Value) -> String {
    let search = text(args, "searchArgument");
    let tag = text(args, "tagArgument");
    if matches!(command, "searchNoteCommand" | "starredNotesCommand") && !tag.trim().is_empty() {
        format!("tag:{tag} {search}").trim().into()
    } else {
        search.into()
    }
}

fn bound_query(value: &str) -> Result<String, String> {
    if value.len() > 1024 || value.contains('\0') {
        Err("Obsidian search is limited to 1024 bytes".into())
    } else {
        Ok(value.to_owned())
    }
}

fn initial_create(state: &State) -> Value {
    let fill = state.prefs["fillFormWithDefaults"] == true;
    let tag = text(&state.prefs, "prefTag");
    json!({"name":if fill{text(&state.prefs,"prefNoteName")}else{""},"path":text(&state.prefs,"prefPath"),"content":if fill{text(&state.prefs,"prefNoteContent")}else{""},"tags":if tag.is_empty(){vec![]}else{vec![tag]}})
}

fn stored_actions(raw: Value) -> Result<Vec<CustomAction>, String> {
    if raw.is_null() {
        return Ok(vec![]);
    }
    let items: Vec<CustomAction> =
        serde_json::from_value(raw).map_err(|_| "Saved Obsidian actions are invalid")?;
    if items.len() > 128 {
        return Err("Obsidian supports up to 128 saved actions".into());
    }
    let mut ids = std::collections::HashSet::new();
    for item in &items {
        item.validate()?;
        if !ids.insert(&item.id) {
            return Err("Duplicate saved action identity".into());
        }
    }
    Ok(items)
}
async fn load_actions(ctx: &ExtensionContext, state: &mut State) -> Result<(), String> {
    state.custom = stored_actions(sdk::read_store(ctx, CUSTOM_STORE).await?["actions"].clone())?;
    Ok(())
}
fn find_action(state: &State, id: &str) -> Result<CustomAction, String> {
    state
        .custom
        .iter()
        .find(|row| row.id == id)
        .cloned()
        .ok_or_else(|| "Saved Obsidian action was not found".into())
}
fn set_form(state: &mut State, kind: Form, draft: Value) {
    state.page = Page::Form { kind, draft };
}

impl Port {
    fn root(&self) -> Result<ExtensionContext, String> {
        self.root
            .lock()
            .map_err(|_| "Obsidian background context unavailable")?
            .clone()
            .ok_or_else(|| "Obsidian process is uninitialized".into())
    }

    async fn new_state(
        &self,
        ctx: &ExtensionContext,
        command: &str,
        payload: &Value,
    ) -> Result<State, String> {
        let windows = self.profile.platform(&ctx.platform)?;
        let profile = self.profile.clone();
        let prefs = payload["preferences"].clone();
        let discovery = prefs.clone();
        let result = work(ctx, move |_| profile.discover(windows, &discovery)).await;
        let mut state = State::new(windows, prefs, payload["argumentValues"].clone(), vec![]);
        match result {
            Ok(vaults) => state.vaults = vaults,
            Err(error) => state.notice = Some(error),
        };
        state.query = bound_query(&query_arg(command, &state.args))?;
        if command == "searchMedia" {
            let extension = text(&state.args, "typeArgument")
                .trim()
                .trim_start_matches('.')
                .to_ascii_lowercase();
            if !extension.is_empty() {
                state.kind = extension;
            }
        }
        Ok(state)
    }

    async fn initial(
        &self,
        ctx: &ExtensionContext,
        command: &str,
        state: &mut State,
        payload: &Value,
    ) -> Transition {
        if command == "runActionCommand" {
            state.page = Page::Actions;
            load_actions(ctx, state).await?;
            let link = text(&payload["launchContext"], "actionId");
            if !link.is_empty() {
                self.run_form(ctx, state, link).await?;
            }
            return Ok(None);
        }
        if state.vaults.is_empty() {
            if state.notice.is_none() {
                state.notice = Some(
                    "No Obsidian vault found. Set Path to Vault or open a vault in Obsidian."
                        .into(),
                );
            }
            return Ok(None);
        }
        if command == "openVaultCommand" {
            return Ok(None);
        }
        let selector = if matches!(
            command,
            "dailyNoteCommand"
                | "dailyNoteAppendCommand"
                | "quickCaptureCommand"
                | "appendTaskCommand"
        ) {
            text(&state.prefs, "vaultName")
        } else {
            ""
        };
        if !selector.is_empty() {
            match vault::resolve_choice(&state.vaults, selector) {
                Ok(index) => return self.choose(ctx, command, state, index).await,
                Err(error) => {
                    state.notice = Some(error);
                    return Ok(None);
                }
            }
        }
        if state.vaults.len() == 1 {
            self.choose(ctx, command, state, 0).await
        } else {
            Ok(None)
        }
    }

    async fn choose(
        &self,
        ctx: &ExtensionContext,
        command: &str,
        state: &mut State,
        index: usize,
    ) -> Transition {
        if index >= state.vaults.len() {
            return Err("Selected vault is no longer available".into());
        }
        state.selected = Some(index);
        match command {
            "searchNoteCommand" | "starredNotesCommand" | "randomNoteCommand" | "searchMedia" => {
                state.page = if command == "searchMedia" {
                    Page::Media
                } else {
                    Page::Notes
                };
                self.rescan(ctx, command, state).await?;
                if command == "randomNoteCommand" {
                    self.random(state, None)?;
                }
            }
            "openWorkspaceCommand" => {
                state.page = Page::Workspaces;
                let vault = state.vault()?;
                if !vault.plugin("obsidian-advanced-uri")? || !vault.plugin("workspaces")? {
                    state.workspaces.clear();
                    state.notice = Some("Enable Workspaces and Advanced URI in this vault".into());
                } else {
                    state.workspaces = vault.workspaces()?.into_iter().collect();
                }
            }
            "dailyNoteCommand" => {
                return reply(ops::daily_open(ctx, state.vault()?, Local::now()).await)
            }
            "dailyNoteAppendCommand" | "appendTaskCommand" => {
                let kind = if command == "dailyNoteAppendCommand" {
                    Form::DailyAppend
                } else {
                    Form::Task
                };
                let draft =
                    json!({"text":text(&state.args,"text"),"dueDate":text(&state.args,"dueDate")});
                set_form(state, kind.clone(), draft.clone());
                if !text(&draft, "text").trim().is_empty() {
                    return reply(self.save(ctx, state, kind, &draft, None).await);
                }
            }
            "quickCaptureCommand" => {
                set_form(state, Form::Capture, json!({"type":"daily","text":""}))
            }
            "createNoteCommand" => {
                set_form(state, Form::Create, initial_create(state));
                if state.vaults.len() == 1 && state.prefs["blankNote"] == true {
                    return reply(
                        ops::blank(ctx, state.vault()?, &state.prefs, Local::now()).await,
                    );
                }
            }
            _ => return Err("Unknown Obsidian vault command".into()),
        }
        Ok(None)
    }

    async fn rescan(
        &self,
        ctx: &ExtensionContext,
        command: &str,
        state: &mut State,
    ) -> Result<(), String> {
        let index = state.selected.ok_or("Select a vault")?;
        let vault = state.vault()?.clone();
        let prefs = state.prefs.clone();
        let media = matches!(state.page, Page::Media);
        state.entries.clear();
        state.hits.clear();
        let (entries, limited) = work(ctx, move |cancel| {
            vault::entries(&vault, index, &prefs, media, cancel)
        })
        .await?;
        state.entries = entries;
        state.scan_limited = limited;
        state.limit = 45;
        self.find(ctx, command, state).await
    }

    async fn find(
        &self,
        ctx: &ExtensionContext,
        command: &str,
        state: &mut State,
    ) -> Result<(), String> {
        let vault = state.vault()?.clone();
        let entries = state.entries.clone();
        let prefs = state.prefs.clone();
        let query = state.query.clone();
        let kind = state.kind.clone();
        let sort = state.sort.clone();
        let starred = command == "starredNotesCommand";
        let (results, limited) = work(ctx, move |cancel| {
            markdown::search(
                &vault, &entries, &query, &prefs, starred, &kind, &sort, cancel,
            )
        })
        .await?;
        state.hits = results;
        state.limited = state.scan_limited || limited;
        Ok(())
    }

    fn random(&self, state: &mut State, previous: Option<&str>) -> Result<(), String> {
        let mut choices: Vec<_> = state
            .entries
            .iter()
            .filter(|entry| entry.markdown() && entry.stamp.bytes <= fs::MAX_DRAFT as u64)
            .collect();
        if choices.len() > 1 {
            choices.retain(|entry| previous != Some(&entry.rel));
        }
        if choices.is_empty() {
            state.notice = Some("No Markdown notes are available for preview".into());
            return Ok(());
        }
        let index = (uuid::Uuid::new_v4().as_u128() % choices.len() as u128) as usize;
        let entry = choices[index].clone();
        self.preview(
            state,
            Hit {
                entry: entry.clone(),
                line: None,
                column: None,
                context: String::new(),
            },
        )
    }

    fn preview(&self, state: &mut State, hit: Hit) -> Result<(), String> {
        let vault = state.vault_for(&hit)?;
        let (raw, _) = ops::preview_content(vault, &hit, fs::MAX_DRAFT)?;
        let content = markdown::clean(&raw, &state.prefs);
        state.page = Page::Preview { hit, content, raw };
        Ok(())
    }

    async fn notes_after(
        &self,
        ctx: &ExtensionContext,
        command: &str,
        state: &mut State,
        hit: &Hit,
    ) -> Result<(), String> {
        state.page = Page::Notes;
        self.rescan(ctx, command, state).await?;
        if command == "randomNoteCommand" {
            if let Some(entry) = state
                .entries
                .iter()
                .find(|entry| entry.rel == hit.entry.rel)
                .cloned()
            {
                self.preview(
                    state,
                    Hit {
                        entry,
                        line: None,
                        column: None,
                        context: String::new(),
                    },
                )?;
            } else {
                self.random(state, None)?;
            }
        }
        Ok(())
    }

    async fn note(
        &self,
        ctx: &ExtensionContext,
        command: &str,
        state: &mut State,
        hit: Hit,
        op: NoteOp,
    ) -> Transition {
        use NoteOp::*;
        let vault = state.vault_for(&hit)?.clone();
        let path = vault.file_display(&hit.entry.rel)?;
        let link = ops::note_uri(&vault, &hit.entry.rel)?;
        let stem = hit
            .entry
            .rel
            .strip_suffix(".md")
            .or_else(|| hit.entry.rel.strip_suffix(".MD"))
            .unwrap_or(&hit.entry.rel);
        let wiki = format!("[[{stem}]]");
        if op != Edit {
            ops::checked_note(&vault, &hit)?;
        }
        match op {
            Preview => self.preview(state, hit)?,
            Open | NewPane => {
                return reply(ops::opened(ctx, &ops::open_hit(&vault, &hit, op == NewPane)?).await)
            }
            DefaultApp => return reply(ops::opened(ctx, &path).await),
            Reveal => {
                ctx.host("host.shell.reveal", json!({"path":path})).await?;
            }
            CopyPath | CopyTitle | Uri | Wiki | MarkdownLink => {
                let value = match op {
                    CopyPath => path,
                    CopyTitle => hit.entry.title,
                    Uri => link,
                    Wiki => wiki,
                    _ => format!("[{}]({link})", hit.entry.title.replace(']', "\\]")),
                };
                sdk::copy(ctx, &value).await?;
            }
            PasteWiki => {
                ctx.host("host.clipboard.pasteText", json!({"text":wiki}))
                    .await?;
                return reply(ops::done(ctx, "Pasted Wikilink").await);
            }
            CopyContent | PasteContent | Code => {
                let (raw, _) = ops::preview_content(&vault, &hit, fs::MAX_DRAFT)?;
                match op {
                    CopyContent => {
                        sdk::copy(ctx, &markdown::clean(&raw, &state.prefs)).await?;
                    }
                    PasteContent => {
                        ctx.host(
                            "host.clipboard.pasteText",
                            json!({"text":markdown::clean(&raw,&state.prefs)}),
                        )
                        .await?;
                        return reply(ops::done(ctx, "Pasted note content").await);
                    }
                    _ => {
                        let blocks = markdown::code_blocks(&raw);
                        if blocks.is_empty() {
                            return Err("No fenced code blocks in the selected note".into());
                        }
                        state.page = Page::Code { hit, blocks };
                    }
                }
            }
            Bookmark | Unbookmark => {
                vault.toggle_bookmark(&hit.entry, op == Bookmark)?;
                self.notes_after(ctx, command, state, &hit).await?;
            }
            Append => {
                let prefill = text(&state.prefs, "appendTemplate").to_owned();
                set_form(state, Form::Append(hit), json!({"content":prefill}));
            }
            AppendSelected => {
                let selected = ctx
                    .host("host.selection.read", json!({"kind":"text"}))
                    .await?;
                let selected = selected
                    .as_str()
                    .filter(|value| !value.trim().is_empty())
                    .ok_or("Select text to append")?;
                let template = text(&state.prefs, "appendSelectedTemplate");
                let template = if template.is_empty() {
                    "{content}"
                } else {
                    template
                };
                let expanded = markdown::expand(ctx, selected, template, Local::now()).await?;
                vault
                    .open()?
                    .append(&hit.entry.rel, &expanded, Some(hit.entry.stamp.identity))
                    .map_err(|e| e.to_string())?;
                state.feedback = Some("Added selected text to note");
                self.notes_after(ctx, command, state, &hit).await?;
            }
            Edit => {
                let (raw, stamp) = ops::preview_content(&vault, &hit, fs::MAX_DRAFT)?;
                set_form(state, Form::Edit(hit, stamp), json!({"content":raw}));
            }
            Trash => {
                let message = format!(
                    "Move {} to the selected vault's .trash folder?",
                    hit.entry.rel
                );
                if ops::confirm(ctx, "Delete Note", &message, "Move to Trash").await? {
                    vault
                        .open()?
                        .trash(&hit.entry.rel, hit.entry.stamp)
                        .map_err(|e| e.to_string())?;
                    self.notes_after(ctx, command, state, &hit).await?;
                }
            }
        }
        Ok(None)
    }

    async fn save(
        &self,
        ctx: &ExtensionContext,
        state: &State,
        kind: Form,
        draft: &Value,
        folder: Option<&str>,
    ) -> CoreResult {
        let vault = state.vault()?;
        let now = Local::now();
        match kind {
            Form::Create => ops::create(ctx, vault, &state.prefs, draft, folder, now).await,
            Form::Capture => ops::capture(ctx, vault, &state.prefs, draft, now).await,
            Form::DailyAppend => ops::daily_append(ctx, vault, &state.prefs, draft, now).await,
            Form::Task => ops::task(ctx, vault, &state.prefs, draft, now).await,
            Form::RunAction(action) => ops::run_action(ctx, vault, &action, draft, now).await,
            _ => Err("Obsidian form does not support this operation".into()),
        }
    }

    async fn run_form(
        &self,
        ctx: &ExtensionContext,
        state: &mut State,
        id: &str,
    ) -> Result<(), String> {
        let action = find_action(state, id)?;
        let selector = if action.vault_path.is_empty() {
            &action.vault_name
        } else {
            &action.vault_path
        };
        state.selected = Some(vault::resolve_choice(&state.vaults, selector)?);
        let start = if action.kind == "template" {
            markdown::expand(ctx, "", &action.template, Local::now()).await?
        } else {
            String::new()
        };
        set_form(state, Form::RunAction(action), json!({"content":start}));
        Ok(())
    }

    fn custom_form(&self, state: &mut State, id: Option<String>) -> Result<(), String> {
        let action = id.as_deref().map(|id| find_action(state, id)).transpose()?;
        let a = action.as_ref();
        let fallback = state.vaults.first().map(|v| v.key()).unwrap_or_default();
        let selector = a
            .map(|r| {
                if r.vault_path.is_empty() {
                    &r.vault_name
                } else {
                    &r.vault_path
                }
            })
            .map_or(fallback.clone(), |key| {
                vault::resolve_choice(&state.vaults, key)
                    .ok()
                    .map(|i| state.vaults[i].key())
                    .unwrap_or_default()
            });
        let draft = json!({"title":a.map(|r|r.title.as_str()).unwrap_or(""),"description":a.map(|r|r.description.as_str()).unwrap_or(""),
            "vault":selector,"path":a.map(|r|r.path.as_str()).unwrap_or(""),"type":a.filter(|r|!r.kind.is_empty()).map(|r|r.kind.as_str()).unwrap_or("capture"),
            "template":a.map(|r|r.template.as_str()).unwrap_or("{content}"),"mode":a.map(|r|r.mode.as_str()).unwrap_or("append"),
            "heading":a.map(|r|r.heading.as_str()).unwrap_or(""),"silent":a.is_some_and(|r|r.silent)});
        state.selected = None;
        set_form(state, Form::ActionEdit(id), draft);
        Ok(())
    }

    async fn custom_save(
        &self,
        ctx: &ExtensionContext,
        state: &mut State,
        id: Option<String>,
        draft: &Value,
    ) -> Result<(), String> {
        let original = id.as_deref().map(|id| find_action(state, id)).transpose()?;
        let selected = vault::resolve_choice(&state.vaults, text(draft, "vault"))?;
        let vault = &state.vaults[selected];
        let record = CustomAction {
            id: id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
            title: text(draft, "title").trim().into(),
            description: text(draft, "description").into(),
            vault_name: vault.name.clone(),
            vault_path: vault.key(),
            path: text(draft, "path").into(),
            template: text(draft, "template").into(),
            kind: text(draft, "type").into(),
            mode: text(draft, "mode").into(),
            heading: text(draft, "heading").into(),
            silent: draft["silent"] == true,
        };
        record.validate()?;
        sdk::update_store(ctx, CUSTOM_STORE, move |store| {
            let mut records = stored_actions(store["actions"].clone())?;
            if let Some(original) = original {
                let row = records
                    .iter_mut()
                    .find(|a| a.id == original.id)
                    .ok_or("Action was removed in another session")?;
                if *row != original {
                    return Err(
                        "Action changed in another session. Reopen it before saving.".into(),
                    );
                }
                *row = record;
            } else {
                if records.len() >= 128 {
                    return Err("Obsidian supports up to 128 saved actions".into());
                }
                records.push(record);
            }
            store["actions"] = json!(records);
            Ok(())
        })
        .await?;
        state.page = Page::Actions;
        state.selected = None;
        load_actions(ctx, state).await
    }

    async fn import(&self, ctx: &ExtensionContext, state: &mut State) -> Result<(), String> {
        let raw = ctx
            .host("host.clipboard.read", json!({"format":"text"}))
            .await?;
        let raw = raw
            .as_str()
            .filter(|value| !value.is_empty() && value.len() <= 512 * 1024)
            .ok_or("Clipboard needs an Obsidian actions JSON array up to 512 KiB")?;
        let mut incoming = stored_actions(
            serde_json::from_str(raw)
                .map_err(|_| "Clipboard does not contain a valid Obsidian actions export")?,
        )?;
        for record in &mut incoming {
            let selector = if record.vault_path.is_empty() {
                &record.vault_name
            } else {
                &record.vault_path
            };
            let index = vault::resolve_choice(&state.vaults, selector)?;
            record.vault_path = state.vaults[index].key();
            record.vault_name = state.vaults[index].name.clone();
        }
        let summary=format!("Merge {} actions into this extension's saved actions? Existing action IDs will be updated.",incoming.len());
        if !ops::confirm(ctx, "Import Actions", &summary, "Import").await? {
            return Ok(());
        }
        sdk::update_store(ctx, CUSTOM_STORE, move |store| {
            let mut all = stored_actions(store["actions"].clone())?;
            for record in incoming {
                if let Some(existing) = all.iter_mut().find(|a| a.id == record.id) {
                    *existing = record;
                } else {
                    all.push(record);
                }
            }
            if all.len() > 128 {
                return Err("Obsidian supports up to 128 saved actions".into());
            }
            store["actions"] = json!(all);
            Ok(())
        })
        .await?;
        load_actions(ctx, state).await
    }

    async fn submit(
        &self,
        ctx: &ExtensionContext,
        command: &str,
        state: &mut State,
        payload: &Value,
        folder: Option<String>,
        expand_append: bool,
    ) -> Transition {
        let Page::Form { kind, draft } = &mut state.page else {
            return Err("Obsidian form is closed".into());
        };
        let fields = payload["formValues"]
            .as_object()
            .ok_or("Form values are missing")?;
        let values = draft.as_object_mut().ok_or("Obsidian draft is invalid")?;
        for (key, value) in fields {
            if values.contains_key(key) {
                values.insert(key.clone(), value.clone());
            }
        }
        if serde_json::to_vec(&draft)
            .map_err(|_| "Form draft is not serializable")?
            .len()
            > 1024 * 1024
        {
            return Err("Obsidian form is too large".into());
        }
        let form = kind.clone();
        let draft = draft.clone();
        match &form {
            Form::Append(hit) => {
                let content = text(&draft, "content");
                if content.len() > fs::MAX_DRAFT {
                    return Err("Obsidian draft exceeds 512 KiB".into());
                }
                let expanded;
                let content = if expand_append {
                    expanded = markdown::expand(ctx, content, "", Local::now()).await?;
                    expanded.as_str()
                } else {
                    content
                };
                if content.trim().is_empty() {
                    return Err("Enter text to append".into());
                }
                state
                    .vault_for(hit)?
                    .open()?
                    .append(&hit.entry.rel, content, Some(hit.entry.stamp.identity))
                    .map_err(|e| e.to_string())?;
                state.feedback = Some("Added text to note");
                self.notes_after(ctx, command, state, hit).await?;
            }
            Form::Edit(hit, stamp) => {
                let content = text(&draft, "content");
                if content.len() > fs::MAX_DRAFT {
                    return Err("Obsidian draft exceeds 512 KiB".into());
                }
                let message = format!("Replace the contents of {} with this draft?", hit.entry.rel);
                if ops::confirm(ctx, "Save Note Changes", &message, "Save Changes").await? {
                    state
                        .vault_for(hit)?
                        .open()?
                        .replace(&hit.entry.rel, content.as_bytes(), *stamp, fs::MAX_DRAFT)
                        .map_err(|e| e.to_string())?;
                    state.feedback = Some("Edited note");
                    self.notes_after(ctx, command, state, hit).await?;
                }
            }
            Form::ActionEdit(id) => self.custom_save(ctx, state, id.clone(), &draft).await?,
            _ => return reply(self.save(ctx, state, form, &draft, folder.as_deref()).await),
        }
        Ok(None)
    }

    async fn invoke(
        &self,
        ctx: &ExtensionContext,
        command: &str,
        state: &mut State,
        payload: &Value,
        action: Callback,
    ) -> Transition {
        match action {
            Callback::Vault(index) => return self.choose(ctx, command, state, index).await,
            Callback::OpenVault(index) => {
                let vault = state.vaults.get(index).ok_or("Unknown vault")?;
                vault.open()?;
                return reply(ops::opened(ctx, &ops::vault_uri(vault)).await);
            }
            Callback::RevealVault(index) | Callback::CopyVault(index) => {
                let vault = state.vaults.get(index).ok_or("Unknown vault")?;
                vault.open()?;
                if matches!(action, Callback::RevealVault(_)) {
                    ctx.host("host.shell.reveal", json!({"path":vault.display()}))
                        .await?;
                } else {
                    sdk::copy(ctx, &vault.display()).await?;
                }
            }
            Callback::Settings => {
                ctx.host(
                    "host.settings.open",
                    json!({"section":"settings-extensions"}),
                )
                .await?;
            }
            Callback::Note(hit, op) => return self.note(ctx, command, state, hit, op).await,
            Callback::Workspace(name) => {
                let vault = state.vault()?;
                if !vault.plugin("obsidian-advanced-uri")?
                    || !vault.plugin("workspaces")?
                    || !vault.workspaces()?.contains_key(&name)
                {
                    return Err("Workspace changed. Refresh and try again.".into());
                }
                let uri = ops::uri(
                    "adv-uri",
                    &[("vault", ops::selector(vault)?), ("workspace", &name)],
                );
                return reply(ops::opened(ctx, &uri).await);
            }
            Callback::Refresh => match state.page {
                Page::Vaults => {
                    let profile = self.profile.clone();
                    let prefs = state.prefs.clone();
                    let windows = state.platform_windows;
                    state.vaults = work(ctx, move |_| profile.discover(windows, &prefs)).await?;
                    return self.initial(ctx, command, state, payload).await;
                }
                Page::Notes | Page::Media => self.rescan(ctx, command, state).await?,
                Page::Workspaces => {
                    return self
                        .choose(ctx, command, state, state.selected.ok_or("Choose a vault")?)
                        .await
                }
                _ => {}
            },
            Callback::Sort | Callback::MediaType => {
                let value = text(&payload["formValues"], "value");
                if matches!(action, Callback::Sort) {
                    if !view::SORTS.iter().any(|(sort, _)| *sort == value) {
                        return Err("Invalid sort order".into());
                    }
                    state.sort = value.into();
                } else {
                    if value != "all" && !state.entries.iter().any(|entry| entry.kind == value) {
                        return Err("Unknown media type".into());
                    }
                    state.kind = value.into();
                }
                state.limit = 45;
                self.find(ctx, command, state).await?;
            }
            Callback::Create => {
                let mut draft = initial_create(state);
                if !state.query.is_empty() && !state.query.starts_with("tag:") {
                    draft["name"] = json!(ui_text(&state.query, 230));
                }
                set_form(state, Form::Create, draft);
            }
            Callback::Another => {
                let previous = match &state.page {
                    Page::Preview { hit, .. } => Some(hit.entry.rel.clone()),
                    _ => None,
                };
                self.random(state, previous.as_deref())?;
            }
            Callback::Back => match &state.page {
                Page::Code { hit, .. } => {
                    let hit = hit.clone();
                    self.preview(state, hit)?;
                }
                Page::Preview { .. } if command != "randomNoteCommand" => {
                    state.page = Page::Notes;
                }
                Page::Form {
                    kind: Form::Append(_) | Form::Edit(..),
                    ..
                } => {
                    state.page = Page::Notes;
                }
                Page::Form {
                    kind: Form::Create, ..
                } if matches!(command, "searchNoteCommand" | "starredNotesCommand") => {
                    state.page = Page::Notes;
                }
                Page::Form {
                    kind: Form::ActionEdit(_) | Form::RunAction(_),
                    ..
                } => {
                    state.selected = None;
                    state.page = Page::Actions;
                }
                _ => {
                    state.selected = None;
                    state.page = Page::Vaults;
                }
            },
            Callback::Submit(folder) => {
                return self
                    .submit(ctx, command, state, payload, folder, false)
                    .await
            }
            Callback::AppendWithTemplate => {
                return self.submit(ctx, command, state, payload, None, true).await
            }
            Callback::CustomNew => self.custom_form(state, None)?,
            Callback::CustomEdit(id) => self.custom_form(state, Some(id))?,
            Callback::CustomRun(id) => self.run_form(ctx, state, &id).await?,
            Callback::CustomDelete(id) => {
                let expected = find_action(state, &id)?;
                if ops::confirm(
                    ctx,
                    "Delete Action",
                    &format!("Delete the saved action {}?", expected.title),
                    "Delete",
                )
                .await?
                {
                    sdk::update_store(ctx, CUSTOM_STORE, move |store| {
                        let mut records = stored_actions(store["actions"].clone())?;
                        let pos = records
                            .iter()
                            .position(|record| record == &expected)
                            .ok_or("Saved action changed. Refresh and confirm again.")?;
                        records.remove(pos);
                        store["actions"] = json!(records);
                        Ok(())
                    })
                    .await?;
                    load_actions(ctx, state).await?;
                }
            }
            Callback::Import => self.import(ctx, state).await?,
            Callback::Export => {
                let data = serde_json::to_string_pretty(&state.custom)
                    .map_err(|_| "Cannot encode saved Obsidian actions")?;
                sdk::copy(ctx, &data).await?;
                state.notice = Some("Copied actions JSON".into());
            }
            Callback::CodeCopy(index) | Callback::CodePaste(index) => {
                let Page::Code { blocks, .. } = &state.page else {
                    return Err("Code preview is closed".into());
                };
                let value = &blocks.get(index).ok_or("Code block was removed")?.1;
                if matches!(action, Callback::CodePaste(_)) {
                    ctx.host("host.clipboard.pasteText", json!({"text":value}))
                        .await?;
                    return reply(ops::done(ctx, "Pasted code").await);
                }
                sdk::copy(ctx, value).await?;
            }
        }
        Ok(None)
    }
}

impl Extension for Port {
    fn id(&self) -> &'static str {
        ID
    }
    fn version(&self) -> &'static str {
        env!("CARGO_PKG_VERSION")
    }
    fn commands(&self) -> &'static [&'static str] {
        COMMANDS
    }
    fn initialize<'a>(&'a self, ctx: &'a ExtensionContext) -> BoxFuture<'a, CoreResult> {
        Box::pin(async move {
            *self
                .root
                .lock()
                .map_err(|_| "Obsidian process context unavailable")? = Some(ctx.clone());
            Ok(json!({"ready":true}))
        })
    }
    fn handle<'a>(
        &'a self,
        ctx: &'a ExtensionContext,
        command: &'a str,
        op: &'a str,
        payload: &'a Value,
        wire_state: &'a mut Value,
    ) -> BoxFuture<'a, CoreResult> {
        Box::pin(async move {
            if op == "extension:dispose-session" {
                *wire_state = json!({});
                return Ok(sdk::unchanged());
            }
            let windows = self.profile.platform(&ctx.platform)?;
            if !COMMANDS.contains(&command) {
                return Err("Unknown Obsidian command".into());
            }
            if command == "obsidianMenuBar" {
                if op != "extension:run-command" {
                    return Err("Invalid status item operation".into());
                }
                self.tray
                    .enable(
                        ctx,
                        &self.root()?,
                        &self.profile,
                        windows,
                        &payload["preferences"],
                    )
                    .await?;
                return Ok(sdk::no_view("Obsidian menu enabled"));
            }
            let mut state = if op == "extension:run-command" {
                self.new_state(ctx, command, payload).await?
            } else {
                serde_json::from_value(wire_state.clone())
                    .map_err(|_| "Obsidian view state unavailable")?
            };
            let transition = match op {
                "extension:run-command" => self.initial(ctx, command, &mut state, payload).await,
                "extension:invoke-action" => {
                    let callback = state.callback(text(payload, "actionId"))?;
                    state.notice = None;
                    self.invoke(ctx, command, &mut state, payload, callback)
                        .await
                }
                "extension:search-text-changed" => {
                    if !matches!(state.page, Page::Notes | Page::Media) {
                        return Ok(sdk::unchanged());
                    }
                    state.query = bound_query(text(payload, "searchText"))?;
                    state.limit = 45;
                    state.notice = None;
                    self.find(ctx, command, &mut state).await.map(|_| None)
                }
                "extension:refresh-session" => return Ok(sdk::unchanged()),
                "extension:load-more" => {
                    state.limit = (state.limit + 45).min(270);
                    Ok(None)
                }
                _ => return Err("Unsupported Obsidian operation".into()),
            };
            match transition {
                Ok(Some(reply)) => {
                    *wire_state = json!({});
                    return Ok(reply);
                }
                Ok(None) => {}
                Err(error) => {
                    state.notice = Some(ui_text(&error, 410));
                }
            }
            let response = view::render(command, &mut state)?;
            *wire_state =
                serde_json::to_value(state).map_err(|_| "Could not retain Obsidian view state")?;
            Ok(response)
        })
    }
    fn background_activate<'a>(
        &'a self,
        ctx: &'a ExtensionContext,
        payload: &'a Value,
    ) -> BoxFuture<'a, CoreResult> {
        Box::pin(async move {
            let windows = self.profile.platform(&ctx.platform)?;
            if payload["key"] != tray::RESOURCE {
                return Err("Unknown Obsidian background resource".into());
            }
            self.tray
                .enable(ctx, &self.root()?, &self.profile, windows, &payload["data"])
                .await
        })
    }
    fn background_event<'a>(
        &'a self,
        ctx: &'a ExtensionContext,
        payload: &'a Value,
    ) -> BoxFuture<'a, CoreResult> {
        Box::pin(async move {
            let windows = self.profile.platform(&ctx.platform)?;
            self.tray.event(ctx, &self.profile, windows, payload).await
        })
    }
}

fn profile() -> Result<Option<Profile>, String> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    match args.as_slice() {
        [] => Ok(Some(Profile::Native)),
        [flag] if flag == "--protocol-info" => {
            println!(
                "{}",
                json!({"id":ID,"version":env!("CARGO_PKG_VERSION"),"protocol":1,"commands":COMMANDS,"fixtureSupport":cfg!(feature="protocol-fixtures")})
            );
            Ok(None)
        }
        #[cfg(feature = "protocol-fixtures")]
        [flag, path] if flag == "--fixtures" => {
            let root = std::path::PathBuf::from(path)
                .canonicalize()
                .map_err(|_| "Synthetic Obsidian profile is missing")?;
            let dir = fs::VaultDir::open(&root, None)
                .map_err(|_| "Synthetic Obsidian profile could not be opened")?;
            if dir
                .read(".tezbar-obsidian-fixture", 72)
                .map_err(|_| "Missing synthetic fixture marker")?
                .0
                != "tezbar-obsidian-fixture-v1\n"
            {
                return Err("Invalid synthetic fixture marker".into());
            }
            Ok(Some(Profile::Fixture(root)))
        }
        _ => Err("Unsupported Obsidian extension option".into()),
    }
}

#[tokio::main(worker_threads = 3)]
async fn main() {
    let result = match profile() {
        Ok(None) => return,
        Ok(Some(profile)) => {
            sdk::serve(Port {
                profile,
                root: Mutex::default(),
                tray: Arc::default(),
            })
            .await
        }
        Err(error) => Err(error),
    };
    if let Err(error) = result {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
