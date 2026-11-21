use crate::{
    markdown::Hit,
    vault::{Entry, Vault},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CustomAction {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub vault_name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub vault_path: String,
    pub path: String,
    #[serde(default)]
    pub template: String,
    #[serde(rename = "type", default)]
    pub kind: String,
    pub mode: String,
    #[serde(default)]
    pub heading: String,
    #[serde(default)]
    pub silent: bool,
}

impl CustomAction {
    pub fn validate(&self) -> Result<(), String> {
        if self.id.is_empty()
            || self.id.len() > 128
            || !self
                .id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
        {
            return Err("Invalid action identity".into());
        }
        if self.title.trim().is_empty() || self.title.len() > 256 {
            return Err("Enter an action title of up to 256 bytes".into());
        }
        if self.path.trim().is_empty() || self.path.len() > 2048 {
            return Err("Enter a relative path for this action".into());
        }
        crate::fs::relative(&self.path)?;
        if !matches!(self.mode.as_str(), "append" | "prepend")
            || !matches!(self.kind.as_str(), "" | "capture" | "template")
        {
            return Err("Custom actions support Capture or Template with Append or Prepend".into());
        }
        if self.template.len() > 32 * 1024
            || self.description.len() > 2048
            || self.heading.len() > 512
            || self.vault_name.len() > 512
            || self.vault_path.len() > 2048
        {
            return Err("Custom action fields exceed their limits".into());
        }
        if self.vault_name.is_empty() && self.vault_path.is_empty() {
            return Err("Select the vault for this action".into());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum Form {
    Create,
    Capture,
    DailyAppend,
    Task,
    Append(Hit),
    Edit(Hit, crate::fs::Stamp),
    ActionEdit(Option<String>),
    RunAction(CustomAction),
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum Page {
    Vaults,
    Notes,
    Media,
    Workspaces,
    Actions,
    Form {
        kind: Form,
        draft: Value,
    },
    Preview {
        hit: Hit,
        content: String,
        raw: String,
    },
    Code {
        hit: Hit,
        blocks: Vec<(String, String)>,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum NoteOp {
    Preview,
    Open,
    DefaultApp,
    NewPane,
    Reveal,
    CopyPath,
    CopyTitle,
    CopyContent,
    PasteContent,
    Wiki,
    PasteWiki,
    MarkdownLink,
    Uri,
    Code,
    Bookmark,
    Unbookmark,
    Append,
    AppendSelected,
    Edit,
    Trash,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum Callback {
    Refresh,
    Back,
    Settings,
    Vault(usize),
    OpenVault(usize),
    RevealVault(usize),
    CopyVault(usize),
    Note(Hit, NoteOp),
    Workspace(String),
    Sort,
    MediaType,
    Create,
    Another,
    Submit(Option<String>),
    AppendWithTemplate,
    Import,
    Export,
    CustomNew,
    CustomRun(String),
    CustomEdit(String),
    CustomDelete(String),
    CodeCopy(usize),
    CodePaste(usize),
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct State {
    pub platform_windows: bool,
    pub prefs: Value,
    pub args: Value,
    pub vaults: Vec<Vault>,
    pub selected: Option<usize>,
    pub page: Page,
    pub query: String,
    pub sort: String,
    pub kind: String,
    pub entries: Vec<Entry>,
    pub hits: Vec<Hit>,
    pub workspaces: Vec<(String, bool)>,
    pub custom: Vec<CustomAction>,
    pub notice: Option<String>,
    #[serde(skip)]
    pub feedback: Option<&'static str>,
    pub limited: bool,
    pub scan_limited: bool,
    pub revision: u64,
    pub callbacks: Vec<Callback>,
    pub limit: usize,
}

impl State {
    pub fn new(windows: bool, prefs: Value, args: Value, vaults: Vec<Vault>) -> Self {
        Self {
            platform_windows: windows,
            prefs,
            args,
            vaults,
            selected: None,
            page: Page::Vaults,
            query: String::new(),
            sort: "relevance".into(),
            kind: "all".into(),
            entries: vec![],
            hits: vec![],
            workspaces: vec![],
            custom: vec![],
            notice: None,
            feedback: None,
            limited: false,
            scan_limited: false,
            revision: 0,
            callbacks: vec![],
            limit: 45,
        }
    }
    pub fn vault(&self) -> Result<&Vault, String> {
        self.selected
            .and_then(|index| self.vaults.get(index))
            .ok_or_else(|| "Choose an Obsidian vault".into())
    }
    pub fn vault_for(&self, hit: &Hit) -> Result<&Vault, String> {
        self.vaults
            .get(hit.entry.vault)
            .ok_or_else(|| "Selected vault is no longer available".into())
    }
    pub fn callback(&self, input: &str) -> Result<Callback, String> {
        let (version, index) = input.split_once(':').ok_or("Invalid Obsidian action")?;
        if version.parse::<u64>().ok() != Some(self.revision) || self.revision == 0 {
            return Err("Stale Obsidian action. Refresh and try again.".into());
        }
        self.callbacks
            .get(
                index
                    .parse::<usize>()
                    .map_err(|_| "Invalid Obsidian action")?,
            )
            .cloned()
            .ok_or_else(|| "Unknown Obsidian action".into())
    }
}
