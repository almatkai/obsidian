//! Explicit wire nodes for the standard Tezbar GPUI extension views.
use crate::{
    markdown::{self, Hit},
    model::{Callback, Form, NoteOp, Page, State},
    text, ui_text,
};
use serde::Serialize;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(1);
pub const SORTS: &[(&str, &str)] = &[
    ("relevance", "Relevance"),
    ("alphabetical-asc", "File name (A to Z)"),
    ("alphabetical-desc", "File name (Z to A)"),
    ("modified-desc", "Modified time (new to old)"),
    ("modified-asc", "Modified time (old to new)"),
    ("created-desc", "Created time (new to old)"),
    ("created-asc", "Created time (old to new)"),
];

#[derive(Serialize)]
enum Kind {
    List,
    #[serde(rename = "List.Item")]
    ListItem,
    #[serde(rename = "List.EmptyView")]
    Empty,
    #[serde(rename = "List.Dropdown")]
    Dropdown,
    #[serde(rename = "List.Dropdown.Item")]
    Choice,
    Grid,
    #[serde(rename = "Grid.Item")]
    GridItem,
    #[serde(rename = "Grid.EmptyView")]
    GridEmpty,
    #[serde(rename = "List.Item.Detail")]
    Inline,
    Detail,
    #[serde(rename = "Detail.Metadata")]
    Metadata,
    #[serde(rename = "Detail.Metadata.Label")]
    Label,
    Form,
    #[serde(rename = "Form.Description")]
    FormDescription,
    #[serde(rename = "Form.TextField")]
    Field,
    #[serde(rename = "Form.TextArea")]
    Area,
    #[serde(rename = "Form.Checkbox")]
    Check,
    #[serde(rename = "Form.Dropdown")]
    FormSelect,
    #[serde(rename = "Form.Dropdown.Item")]
    SelectItem,
    #[serde(rename = "Form.TagPicker")]
    Tags,
    #[serde(rename = "Form.TagPicker.Item")]
    Tag,
}

#[derive(Serialize)]
struct Node {
    #[serde(rename = "type")]
    kind: Kind,
    props: Value,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    children: Vec<Node>,
    #[serde(skip_serializing_if = "Option::is_none")]
    metadata: Option<Box<Node>>,
}
fn node(kind: Kind, props: Value, children: Vec<Node>) -> Node {
    Node {
        kind,
        props,
        children,
        metadata: None,
    }
}

struct Wire {
    revision: u64,
    callbacks: Vec<Callback>,
    actions: Vec<Value>,
    windows: bool,
}
impl Wire {
    fn add(&mut self, label: impl AsRef<str>, callback: Callback) -> String {
        self.with(label, callback, "callback", None, None)
    }
    fn shortcut(
        &mut self,
        label: impl AsRef<str>,
        callback: Callback,
        section: &str,
        key: &str,
        shift: bool,
    ) -> String {
        let mut modifiers = vec![if self.windows { "ctrl" } else { "cmd" }];
        if shift {
            modifiers.push("shift");
        }
        self.with(
            label,
            callback,
            "callback",
            Some(section),
            Some(json!({"modifiers":modifiers,"key":key})),
        )
    }
    fn with(
        &mut self,
        label: impl AsRef<str>,
        callback: Callback,
        kind: &str,
        section: Option<&str>,
        shortcut: Option<Value>,
    ) -> String {
        let id = format!("{}:{}", self.revision, self.callbacks.len());
        self.callbacks.push(callback);
        let mut action = json!({"id":id,"kind":kind,"title":label.as_ref()});
        if let Some(section) = section {
            action["section"] = json!(section);
        }
        if let Some(shortcut) = shortcut {
            action["shortcut"] = shortcut;
        }
        self.actions.push(action);
        id
    }
}

fn title(command: &str) -> &'static str {
    match command {
        "searchNoteCommand" => "Search Notes",
        "starredNotesCommand" => "Search Starred Notes",
        "openVaultCommand" => "Open Vault",
        "openWorkspaceCommand" => "Open Workspace",
        "dailyNoteCommand" => "Open Daily Note",
        "dailyNoteAppendCommand" => "Append to Daily Note",
        "createNoteCommand" => "Create Note",
        "randomNoteCommand" => "Random Note",
        "searchMedia" => "Search Media",
        "appendTaskCommand" => "Append Task",
        "runActionCommand" => "Run Action",
        "quickCaptureCommand" => "Quick Capture",
        _ => "Obsidian",
    }
}

fn choice(
    wire: &mut Wire,
    label: &str,
    value: &str,
    callback: Callback,
    items: impl IntoIterator<Item = (String, String)>,
) -> Node {
    let id = wire.add(label, callback);
    node(
        Kind::Dropdown,
        json!({"actionId":id,"value":value,"tooltip":label}),
        items
            .into_iter()
            .map(|(value, title)| node(Kind::Choice, json!({"value":value,"title":title}), vec![]))
            .collect(),
    )
}

fn note_actions(state: &State, hit: &Hit, wire: &mut Wire, preview: bool) -> Vec<String> {
    let advanced = state.vault_for(hit).ok().is_some_and(|vault| {
        vault.id.is_some() && vault.plugin("obsidian-advanced-uri").unwrap_or(false)
    });
    note_actions_with_advanced(state, hit, wire, preview, advanced)
}

fn note_actions_with_advanced(
    state: &State,
    hit: &Hit,
    wire: &mut Wire,
    preview: bool,
    available: bool,
) -> Vec<String> {
    use NoteOp::*;
    let action = |op| Callback::Note(hit.clone(), op);
    let preferred = match text(&state.prefs, "primaryAction") {
        "quicklook" if hit.entry.markdown() && !preview => Preview,
        "defaultapp" => DefaultApp,
        "newpane" if available => NewPane,
        _ => Open,
    };
    let mut methods = vec![
        (
            Open,
            if hit.line.is_some() {
                "Open Match in Obsidian"
            } else {
                "Open in Obsidian"
            },
        ),
        (DefaultApp, "Open in Default App"),
    ];
    if available {
        methods.push((NewPane, "Open in New Obsidian Tab"));
    }
    if hit.entry.markdown() && !preview {
        methods.push((Preview, "Quick Look"));
    }
    methods.sort_by_key(|(op, _)| *op != preferred);
    let mut ids: Vec<_> = methods
        .into_iter()
        .map(|(op, label)| wire.add(label, action(op)))
        .collect();
    ids.push(wire.add(
        if state.platform_windows {
            "Show in Explorer"
        } else {
            "Show in Finder"
        },
        action(Reveal),
    ));
    ids.push(wire.shortcut("Copy File Path", action(CopyPath), "Copy", "c", true));
    ids.push(wire.add("Copy Note Title", action(CopyTitle)));
    ids.push(wire.add("Copy Obsidian Link", action(Uri)));
    ids.push(wire.add("Copy Markdown Link", action(MarkdownLink)));
    ids.push(wire.add("Copy Wikilink", action(Wiki)));
    ids.push(wire.add("Paste Wikilink", action(PasteWiki)));
    let (label, op) = if hit.entry.starred {
        ("Unbookmark Note", Unbookmark)
    } else {
        ("Bookmark Note", Bookmark)
    };
    ids.push(wire.add(label, action(op)));
    if hit.entry.markdown() {
        ids.push(wire.shortcut("Copy Note Content", action(CopyContent), "Copy", "c", false));
        ids.push(wire.add("Paste Note Content", action(PasteContent)));
        ids.push(wire.add("Copy Code", action(Code)));
        ids.push(wire.add("Append to Note", action(Append)));
        ids.push(wire.add("Append Selected Text to Note", action(AppendSelected)));
        if hit.entry.stamp.bytes <= crate::fs::MAX_DRAFT as u64 {
            ids.push(wire.add("Edit Note", action(Edit)));
        }
    }
    ids.push(wire.with("Delete Note", action(Trash), "callback", Some("Note"), None));
    ids
}

fn clipped(raw: &str, limit: usize) -> &str {
    let mut end = raw.len().min(limit);
    while !raw.is_char_boundary(end) {
        end -= 1;
    }
    &raw[..end]
}

fn metadata(hit: &Hit, raw: &str) -> Node {
    let tags = markdown::tags(raw).join(", ");
    let words = raw.split_whitespace().count();
    let fields = [
        ("File", hit.entry.rel.clone()),
        ("Tags", ui_text(&tags, 380)),
        ("Words", words.to_string()),
        ("Reading Time", format!("{} min", words.div_ceil(220))),
        ("Characters", raw.chars().count().to_string()),
        (
            "Size",
            format!("{:.1} KiB", hit.entry.stamp.bytes as f64 / 1024.0),
        ),
    ];
    node(
        Kind::Metadata,
        json!({}),
        fields
            .into_iter()
            .filter(|(_, value)| !value.is_empty())
            .map(|(title, text)| node(Kind::Label, json!({"title":title,"text":text}), vec![]))
            .collect(),
    )
}

fn item(id: String, title: &str, subtitle: &str, ids: Vec<String>) -> Node {
    node(
        Kind::ListItem,
        json!({"id":id,"title":ui_text(title,470),"subtitle":ui_text(subtitle,420),"actionIds":ids}),
        vec![],
    )
}

fn list(
    state: &State,
    title: &str,
    global: Vec<String>,
    accessory: Option<Node>,
    mut children: Vec<Node>,
    grid: bool,
    server: bool,
) -> Result<Node, String> {
    if let Some(notice) = &state.notice {
        children.insert(0, item("notice".into(), notice, "", global.clone()));
    }
    if children.is_empty() {
        children.push(node(if grid{Kind::GridEmpty}else{Kind::Empty},json!({"title":"No results","description":"Change the search or select another vault.","actionIds":global}),vec![]));
    }
    let columns = match text(&state.prefs, "imageSize") {
        "small" => 8,
        "large" => 3,
        _ => 5,
    };
    let mut props = json!({"searchBarPlaceholder":title,"searchText":if server {&state.query}else{""},"filtering":!server,"__hasServerSearch":server,"actionIds":global,
        "isShowingDetail":matches!(state.page,Page::Notes)&&state.prefs["showDetail"]!=false});
    if grid {
        props["columns"] = json!(columns);
        props["aspectRatio"] = json!("4/3");
    }
    if let Some(accessory) = accessory {
        props["searchBarAccessory"] = serde_json::to_value(accessory).map_err(|e| e.to_string())?;
    }
    Ok(node(
        if grid { Kind::Grid } else { Kind::List },
        props,
        children,
    ))
}

fn field(draft: &Value, id: &str, title: &str, placeholder: &str, area: bool, focus: bool) -> Node {
    node(
        if area { Kind::Area } else { Kind::Field },
        json!({"id":id,"title":title,"placeholder":placeholder,"defaultValue":text(draft,id),"autoFocus":focus}),
        vec![],
    )
}
fn select(
    draft: &Value,
    id: &str,
    title: &str,
    options: impl IntoIterator<Item = (String, String)>,
) -> Node {
    node(
        Kind::FormSelect,
        json!({"id":id,"title":title,"defaultValue":text(draft,id)}),
        options
            .into_iter()
            .map(|(value, title)| {
                node(
                    Kind::SelectItem,
                    json!({"title":title,"value":value}),
                    vec![],
                )
            })
            .collect(),
    )
}
fn options(values: &[(&str, &str)]) -> Vec<(String, String)> {
    values
        .iter()
        .map(|(value, title)| (value.to_string(), title.to_string()))
        .collect()
}
fn description(message: &str) -> Node {
    node(Kind::FormDescription, json!({"text":message}), vec![])
}

fn form(state: &State, form: &Form, draft: &Value, wire: &mut Wire) -> (String, Node) {
    let mut fields = Vec::new();
    if let Some(notice) = &state.notice {
        fields.push(description(notice));
    }
    let mut extras = vec![];
    let (title, label) = match form {
        Form::Create => {
            fields.push(field(draft, "name", "Name", "Name of note", false, false));
            fields.push(field(
                draft,
                "path",
                "Path",
                "Vault folder (optional)",
                false,
                false,
            ));
            let mut tags = crate::vault::parse_list(text(&state.prefs, "tags"));
            let pref = text(&state.prefs, "prefTag");
            if !pref.is_empty() && !tags.iter().any(|s| s == pref) {
                tags.push(pref.to_owned());
            }
            fields.push(node(
                Kind::Tags,
                json!({"id":"tags","title":"Tags","defaultValue":draft["tags"]}),
                tags.into_iter()
                    .map(|tag| node(Kind::Tag, json!({"title":tag,"value":tag}), vec![]))
                    .collect(),
            ));
            fields.push(field(
                draft,
                "content",
                "Content",
                "Markdown",
                true,
                state.prefs["focusContentArea"] == true,
            ));
            for (n, folder) in crate::vault::parse_list(text(&state.prefs, "folderActions"))
                .into_iter()
                .take(9)
                .enumerate()
            {
                let mods = if state.platform_windows {
                    vec!["ctrl", "shift"]
                } else {
                    vec!["cmd", "shift"]
                };
                extras.push(wire.with(
                    format!("Create in {folder}"),
                    Callback::Submit(Some(folder)),
                    "submit-form",
                    Some("Create"),
                    Some(json!({"modifiers":mods,"key":(n+1).to_string()})),
                ));
            }
            ("Create Note".into(), "Create")
        }
        Form::Capture => {
            fields.push(select(
                draft,
                "type",
                "Type",
                options(&[
                    ("daily", "Daily Note"),
                    ("todo", "To Do"),
                    ("shopping", "Shopping"),
                ]),
            ));
            fields.push(field(
                draft,
                "text",
                "Text",
                "Write something to save",
                true,
                true,
            ));
            ("Quick Capture".into(), "Save")
        }
        Form::DailyAppend | Form::Task => {
            fields.push(field(draft, "text", "Text", "Text to append", true, true));
            if matches!(form, Form::Task) {
                fields.push(field(
                    draft,
                    "dueDate",
                    "Due Date",
                    "YYYY-MM-DD",
                    false,
                    false,
                ));
            }
            (
                if matches!(form, Form::Task) {
                    "Append Task"
                } else {
                    "Append to Daily Note"
                }
                .into(),
                "Save",
            )
        }
        Form::Append(hit) | Form::Edit(hit, _) => {
            fields.push(field(draft, "content", "Content", "Markdown", true, true));
            (
                format!(
                    "{} {}",
                    if matches!(form, Form::Edit(..)) {
                        "Edit"
                    } else {
                        "Add Text to"
                    },
                    hit.entry.title
                ),
                "Save",
            )
        }
        Form::ActionEdit(id) => {
            fields.push(field(
                draft,
                "title",
                "Action Title",
                "Log meeting",
                false,
                false,
            ));
            fields.push(field(
                draft,
                "description",
                "Description",
                "Optional",
                false,
                false,
            ));
            fields.push(select(
                draft,
                "vault",
                "Vault",
                state
                    .vaults
                    .iter()
                    .map(|v| (v.key(), format!("{} ({})", v.name, v.display()))),
            ));
            fields.push(field(
                draft,
                "path",
                "Note Path",
                "Daily Notes/{date}.md",
                false,
                false,
            ));
            fields.push(select(
                draft,
                "type",
                "Action Type",
                options(&[
                    ("capture", "Capture text"),
                    ("template", "Pre-fill with template"),
                ]),
            ));
            fields.push(field(
                draft,
                "template",
                "Template",
                "{content}",
                true,
                false,
            ));
            fields.push(select(
                draft,
                "mode",
                "Mode",
                options(&[("append", "Append"), ("prepend", "Prepend")]),
            ));
            fields.push(field(
                draft,
                "heading",
                "Target Heading",
                "Optional heading",
                false,
                false,
            ));
            fields.push(node(Kind::Check,json!({"id":"silent","label":"Save without opening Obsidian","defaultValue":draft["silent"]==true}),vec![]));
            (
                if id.is_some() {
                    "Edit Action"
                } else {
                    "Create Action"
                }
                .into(),
                "Save Action",
            )
        }
        Form::RunAction(action) => {
            if !action.description.is_empty() {
                fields.push(description(&action.description));
            }
            fields.push(field(draft, "content", "Content", "Text", true, true));
            (action.title.clone(), "Run Action")
        }
    };
    let save = wire.with(
        label,
        Callback::Submit(None),
        "submit-form",
        Some("Form"),
        None,
    );
    if matches!(form, Form::Append(_)) && !text(&state.prefs, "appendTemplate").is_empty() {
        extras.push(wire.with(
            "Save with Template Expansion",
            Callback::AppendWithTemplate,
            "submit-form",
            Some("Form"),
            None,
        ));
    }
    let back = wire.add("Back", Callback::Back);
    let mut ids = vec![save];
    ids.extend(extras);
    ids.push(back);
    if let Ok(vault) = state.vault() {
        fields.insert(0, description(&format!("Vault: {}", vault.display())));
    }
    (
        title.clone(),
        node(
            Kind::Form,
            json!({"navigationTitle":title,"actionIds":ids}),
            fields,
        ),
    )
}

pub fn render(command: &str, state: &mut State) -> Result<Value, String> {
    let revision = NEXT.fetch_add(1, Ordering::Relaxed);
    let mut wire = Wire {
        revision,
        callbacks: vec![],
        actions: vec![],
        windows: state.platform_windows,
    };
    let mut title = title(command).to_owned();
    let root = match &state.page {
        Page::Vaults => {
            let mut items = vec![];
            for (index, vault) in state.vaults.iter().enumerate() {
                let ids = if command == "openVaultCommand" {
                    vec![
                        wire.add("Open Vault", Callback::OpenVault(index)),
                        wire.add(
                            if state.platform_windows {
                                "Show in Explorer"
                            } else {
                                "Show in Finder"
                            },
                            Callback::RevealVault(index),
                        ),
                        wire.add("Copy Vault Path", Callback::CopyVault(index)),
                    ]
                } else {
                    vec![wire.add("Select Vault", Callback::Vault(index))]
                };
                items.push(item(
                    format!("vault:{index}"),
                    &vault.name,
                    &vault.display(),
                    ids,
                ));
            }
            let global = vec![
                wire.add("Reload Vaults", Callback::Refresh),
                wire.add("Open Extension Settings", Callback::Settings),
            ];
            list(
                state,
                "Search Obsidian vaults",
                global,
                None,
                items,
                false,
                false,
            )?
        }
        Page::Notes | Page::Media => {
            let media = matches!(state.page, Page::Media);
            let mut global = vec![wire.add("Refresh", Callback::Refresh)];
            if state.vaults.len() > 1 {
                global.push(wire.add("Choose Vault", Callback::Back));
            }
            if !media && command != "randomNoteCommand" {
                global.push(wire.add("Create Note", Callback::Create));
            }
            let accessory = if media {
                let mut kinds = state
                    .entries
                    .iter()
                    .map(|entry| entry.kind.clone())
                    .collect::<Vec<_>>();
                kinds.sort();
                kinds.dedup();
                Some(choice(
                    &mut wire,
                    "Filter by Type",
                    &state.kind,
                    Callback::MediaType,
                    std::iter::once(("all".into(), "All".into()))
                        .chain(kinds.into_iter().map(|v| (v.clone(), format!(".{v}")))),
                ))
            } else if command == "randomNoteCommand" {
                None
            } else {
                Some(choice(
                    &mut wire,
                    "Sort Notes",
                    &state.sort,
                    Callback::Sort,
                    options(SORTS),
                ))
            };
            let mut rows = Vec::new();
            let selected_vault = state.vault().ok();
            let preview_dir = selected_vault
                .filter(|_| state.prefs["showDetail"] != false && !media)
                .and_then(|v| v.open().ok());
            let mut inline_budget = 496 * 1024;
            let advanced = selected_vault.is_some_and(|v| {
                v.id.is_some() && v.plugin("obsidian-advanced-uri").unwrap_or(false)
            });
            for (index, hit) in state.hits.iter().take(state.limit).enumerate() {
                let row_id = format!(
                    "{}:{}:{}:{}",
                    hit.entry.vault,
                    hit.entry.rel,
                    hit.line.unwrap_or(0),
                    hit.column.unwrap_or(0)
                );
                if media {
                    let note = |op| Callback::Note(hit.clone(), op);
                    let ids = vec![
                        wire.add("Open Media", note(NoteOp::DefaultApp)),
                        wire.add("Open in Obsidian", note(NoteOp::Open)),
                        wire.add(
                            if state.platform_windows {
                                "Show in Explorer"
                            } else {
                                "Show in Finder"
                            },
                            note(NoteOp::Reveal),
                        ),
                        wire.add("Copy File Path", note(NoteOp::CopyPath)),
                    ];
                    rows.push(node(Kind::GridItem,json!({"id":row_id,"title":ui_text(&hit.entry.title,330),"subtitle":ui_text(&hit.entry.rel,350),"content":{"source":"resources/extension_icon.png"},"actionIds":ids}),vec![]));
                    continue;
                }
                let mut row = item(
                    row_id,
                    &hit.entry.title,
                    if hit.context.is_empty() {
                        &hit.entry.rel
                    } else {
                        &hit.context
                    },
                    note_actions_with_advanced(state, hit, &mut wire, false, advanced),
                );
                if hit.entry.starred || hit.line.is_some() {
                    let mut accessories = vec![];
                    if let Some(line) = hit.line {
                        accessories
                            .push(json!({"text":format!("L{}:{}",line,hit.column.unwrap_or(1))}));
                    }
                    if hit.entry.starred {
                        accessories.push(json!({"tag":"Starred"}));
                    }
                    row.props["accessories"] = json!(accessories);
                }
                if state.prefs["showDetail"] != false {
                    if let Some(dir) = &preview_dir {
                        if hit.entry.markdown()
                            && hit.entry.stamp.bytes <= 24 * 1024
                            && inline_budget >= hit.entry.stamp.bytes as usize
                            && index < 45
                        {
                            if let Ok((raw, _)) = dir.read(&hit.entry.rel, 24 * 1024) {
                                inline_budget = inline_budget.saturating_sub(raw.len());
                                let preview = markdown::clean(&raw, &state.prefs);
                                let mut detail = node(
                                    Kind::Inline,
                                    json!({"markdown":clipped(&preview,3500)}),
                                    vec![],
                                );
                                if state.prefs["showMetadata"] == true {
                                    detail.metadata = Some(Box::new(metadata(hit, &raw)));
                                }
                                row.children.push(detail);
                            }
                        }
                    }
                }
                rows.push(row);
            }
            let more = state.hits.len() > state.limit && state.limit < 270;
            if state.limited || state.hits.len() > 270 && !more {
                rows.push(item(
                    "limit".into(),
                    "Narrow the search to see more results",
                    "",
                    global.clone(),
                ));
            }
            let mut root = list(
                state,
                if media {
                    "Search Media"
                } else {
                    "Search Notes"
                },
                global,
                accessory,
                rows,
                media,
                true,
            )?;
            root.props["pagination"] = json!({"hasMore":more});
            root
        }
        Page::Workspaces => {
            let mut global = vec![wire.add("Refresh", Callback::Refresh)];
            if state.vaults.len() > 1 {
                global.push(wire.add("Choose Vault", Callback::Back));
            }
            let rows = state
                .workspaces
                .iter()
                .enumerate()
                .map(|(i, (name, active))| {
                    item(
                        format!("workspace:{i}"),
                        name,
                        if *active { "Active" } else { "" },
                        vec![wire.add("Open Workspace", Callback::Workspace(name.clone()))],
                    )
                })
                .collect();
            list(state, "Search Workspaces", global, None, rows, false, false)?
        }
        Page::Actions => {
            let global = vec![
                wire.add("Create New Action", Callback::CustomNew),
                wire.add("Import Actions", Callback::Import),
                wire.add("Export Actions", Callback::Export),
            ];
            let rows = state
                .custom
                .iter()
                .map(|a| {
                    let mut ids = vec![
                        wire.add("Run Action", Callback::CustomRun(a.id.clone())),
                        wire.add("Edit Action", Callback::CustomEdit(a.id.clone())),
                        wire.add("Delete Action", Callback::CustomDelete(a.id.clone())),
                    ];
                    ids.extend(global.clone());
                    item(a.id.clone(), &a.title, &a.path, ids)
                })
                .collect();
            list(state, "Search Actions", global, None, rows, false, false)?
        }
        Page::Form { kind, draft } => {
            let (display, root) = form(state, kind, draft, &mut wire);
            title = display;
            root
        }
        Page::Preview { hit, content, raw } => {
            title = hit.entry.title.clone();
            let mut ids = note_actions(state, hit, &mut wire, true);
            if command == "randomNoteCommand" {
                ids.push(wire.add("Another Note", Callback::Another));
            }
            ids.push(wire.add("Back", Callback::Back));
            let mut body = content.clone();
            if let Some(notice) = &state.notice {
                body = format!("**{}**\n\n{body}", ui_text(notice, 240));
            }
            let mut root = node(
                Kind::Detail,
                json!({"markdown":body,"actionIds":ids}),
                vec![],
            );
            if state.prefs["showMetadata"] == true {
                root.metadata = Some(Box::new(metadata(hit, raw)));
            }
            root
        }
        Page::Code { hit, blocks } => {
            title = format!("Code in {}", hit.entry.title);
            let rows = blocks
                .iter()
                .enumerate()
                .map(|(i, (lang, body))| {
                    item(
                        format!("code:{i}"),
                        body,
                        lang,
                        vec![
                            wire.add("Copy Code", Callback::CodeCopy(i)),
                            wire.add("Paste Code", Callback::CodePaste(i)),
                        ],
                    )
                })
                .collect();
            let global = vec![wire.add("Back", Callback::Back)];
            list(state, "Code blocks", global, None, rows, false, false)?
        }
    };
    let mut view = tezbar_extension_sdk::view(
        &title,
        serde_json::to_value(root).map_err(|e| e.to_string())?,
        wire.actions,
    );
    if let Some(message) = state.feedback.take() {
        view["effects"] = json!([{"kind":"toast","title":message,"style":"success"}]);
        view["effectSequence"] = json!(revision);
    }
    state.callbacks = wire.callbacks;
    state.revision = revision;
    Ok(view)
}
