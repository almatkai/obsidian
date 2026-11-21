use crate::{
    fs::{self, MAX_DRAFT, MAX_NOTE},
    markdown::{self, Hit},
    model::CustomAction,
    text,
    vault::Vault,
};
use chrono::{DateTime, Local};
use serde_json::{json, Value};
use std::io::ErrorKind;
use tezbar_extension_sdk::{self as sdk, CoreResult, ExtensionContext};

pub fn uri(action: &str, params: &[(&str, &str)]) -> String {
    let args = params
        .iter()
        .map(|(key, val)| format!("{key}={}", sdk::encode_component(val)))
        .collect::<Vec<_>>()
        .join("&");
    format!("obsidian://{action}?{args}")
}

pub fn vault_uri(vault: &Vault) -> String {
    if let Some(id) = &vault.id {
        uri("open", &[("vault", id)])
    } else {
        uri("open", &[("path", &vault.display())])
    }
}

pub fn note_uri(vault: &Vault, path: &str) -> Result<String, String> {
    let relative = fs::relative(path)?;
    if let Some(id) = &vault.id {
        Ok(uri("open", &[("vault", id), ("file", &relative)]))
    } else {
        Ok(uri("open", &[("path", &vault.file_display(&relative)?)]))
    }
}

pub fn selector(vault: &Vault) -> Result<&str, String> {
    vault
        .id
        .as_deref()
        .ok_or_else(|| "Open this vault in Obsidian to register it for Advanced URI actions".into())
}

pub fn checked_note(vault: &Vault, hit: &Hit) -> Result<(), String> {
    let current = vault
        .open()?
        .stamp(&hit.entry.rel)
        .map_err(|e| e.to_string())?;
    if current.identity != hit.entry.stamp.identity {
        return Err("Selected file was replaced. Refresh the view.".into());
    }
    Ok(())
}

pub fn open_hit(vault: &Vault, hit: &Hit, tab: bool) -> Result<String, String> {
    checked_note(vault, hit)?;
    if (tab || hit.line.is_some())
        && vault.id.is_some()
        && vault.plugin("obsidian-advanced-uri").unwrap_or(false)
    {
        let selected = selector(vault)?;
        let line = hit.line.unwrap_or(1).to_string();
        let column = hit.column.unwrap_or(1).to_string();
        let mut fields = vec![("vault", selected), ("filepath", hit.entry.rel.as_str())];
        if tab {
            fields.push(("openmode", "tab"));
        }
        if hit.line.is_some() {
            fields.push(("line", &line));
            fields.push(("column", &column));
        }
        Ok(uri("adv-uri", &fields))
    } else if tab {
        Err("Enable Advanced URI and register the vault in Obsidian to open a new tab".into())
    } else {
        note_uri(vault, &hit.entry.rel)
    }
}

pub async fn confirm(
    ctx: &ExtensionContext,
    title: &str,
    message: &str,
    label: &str,
) -> Result<bool, String> {
    Ok(ctx
        .host(
            "host.dialog.confirm",
            json!({"title":title,"message":message,"buttons":["Cancel",label],"type":"warning"}),
        )
        .await?["response"]
        == 1)
}

pub async fn done(ctx: &ExtensionContext, message: &str) -> CoreResult {
    let _ = ctx
        .host("host.window.visibility", json!({"visible":false}))
        .await;
    Ok(sdk::no_view(message))
}

pub async fn opened(ctx: &ExtensionContext, target: &str) -> CoreResult {
    sdk::open(ctx, target).await?;
    done(ctx, "Opened in Obsidian").await
}

pub async fn saved(ctx: &ExtensionContext, vault: &Vault, path: &str, open: bool) -> CoreResult {
    let mut message = "Saved to Obsidian";
    if open {
        match note_uri(vault, path) {
            Ok(url) if sdk::open(ctx, &url).await.is_ok() => {}
            _ => message = "Note saved. Obsidian could not open the link.",
        }
    }
    done(ctx, message).await
}

fn no_conflict(result: std::io::Result<()>) -> Result<(), String> {
    result.map_err(|error| {
        if error.kind() == ErrorKind::AlreadyExists {
            "A note already exists at this path. Choose a different name or folder.".into()
        } else {
            error.to_string()
        }
    })
}

pub async fn create(
    ctx: &ExtensionContext,
    vault: &Vault,
    prefs: &Value,
    form: &Value,
    folder: Option<&str>,
    now: DateTime<Local>,
) -> CoreResult {
    let raw_title = text(form, "name");
    let name = if raw_title.trim().is_empty() {
        prefs["prefNoteName"].as_str().unwrap_or("Untitled")
    } else {
        raw_title
    };
    let expanded = markdown::expand(ctx, name, "", now).await?;
    if expanded.trim().is_empty() || expanded.contains(['/', '\\']) || expanded.starts_with('.') {
        return Err("Enter a visible note name without a folder separator".into());
    }
    let folder = folder.unwrap_or_else(|| text(form, "path"));
    let folder = markdown::expand(ctx, folder, "", now).await?;
    let rel = fs::note_path(
        &if folder.trim().is_empty() {
            expanded
        } else {
            format!("{folder}/{expanded}")
        },
        &vault.config,
        true,
    )?;
    let raw = text(form, "content");
    let content = if raw.is_empty() && prefs["fillFormWithDefaults"] != true {
        text(prefs, "prefNoteContent")
    } else {
        raw
    };
    let content = markdown::expand(ctx, content, "", now).await?;
    let tags = form["tags"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .take(128)
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let content = format!("{}{content}", markdown::frontmatter_for(&tags)?);
    if content.len() > MAX_DRAFT {
        return Err("Note content exceeds 512 KiB".into());
    }
    no_conflict(vault.open()?.create(&rel, content.as_bytes()))?;
    saved(ctx, vault, &rel, prefs["openOnCreate"] != false).await
}

pub async fn blank(
    ctx: &ExtensionContext,
    vault: &Vault,
    prefs: &Value,
    now: DateTime<Local>,
) -> CoreResult {
    let settings = vault.app_settings()?;
    let default = if settings["newFileLocation"] == "folder" {
        text(&settings, "newFileFolderPath")
    } else {
        ""
    };
    let name = if text(prefs, "prefNoteName").trim().is_empty() {
        "Blank Note"
    } else {
        text(prefs, "prefNoteName")
    };
    let name = markdown::expand(ctx, name, "", now).await?;
    if name.contains(['/', '\\']) {
        return Err("Enter a note name without a folder separator".into());
    }
    let rel = fs::note_path(
        &if default.is_empty() {
            name
        } else {
            format!("{default}/{name}")
        },
        &vault.config,
        true,
    )?;
    no_conflict(vault.open()?.create(&rel, b""))?;
    saved(ctx, vault, &rel, true).await
}

pub fn insert(
    vault: &Vault,
    path: &str,
    content: &str,
    heading: &str,
    prepend: bool,
    allow_create: bool,
    seed: &str,
) -> Result<(), String> {
    if content.trim().is_empty() || content.len() > MAX_DRAFT {
        return Err("Enter note content up to 512 KiB".into());
    }
    let path = fs::note_path(path, &vault.config, false)?;
    let root = vault.open()?;
    match root.stamp(&path) {
        Ok(stamp) if heading.trim().is_empty() && !prepend => root
            .append(&path, content, Some(stamp.identity))
            .map_err(|e| e.to_string()),
        Ok(_) => {
            let (original, stamp) = root.read(&path, MAX_NOTE).map_err(|e| e.to_string())?;
            let changed = markdown::inserted(&original, content, heading, prepend)?;
            root.replace(&path, changed.as_bytes(), stamp, MAX_NOTE)
                .map_err(|e| e.to_string())
        }
        Err(error) if error.kind() == ErrorKind::NotFound && allow_create => {
            let changed = markdown::inserted(seed, content, heading, prepend)?;
            no_conflict(root.create(&path, changed.as_bytes()))
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {
            Err("The selected Markdown note does not exist".into())
        }
        Err(error) => Err(error.to_string()),
    }
}

pub async fn daily_open(ctx: &ExtensionContext, vault: &Vault, now: DateTime<Local>) -> CoreResult {
    let (path, seed) = markdown::daily_target(vault, now)?;
    let root = vault.open()?;
    if let Err(error) = root.stamp(&path) {
        if error.kind() != ErrorKind::NotFound {
            return Err(error.to_string());
        }
        match root.create(&path, seed.as_bytes()) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                root.stamp(&path).map_err(|e| e.to_string())?;
            }
            Err(error) => return Err(error.to_string()),
        }
    }
    opened(ctx, &note_uri(vault, &path)?).await
}

pub async fn daily_append(
    ctx: &ExtensionContext,
    vault: &Vault,
    prefs: &Value,
    draft: &Value,
    now: DateTime<Local>,
) -> CoreResult {
    let content =
        markdown::expand(ctx, text(draft, "text"), text(prefs, "appendTemplate"), now).await?;
    let (path, seed) = markdown::daily_target(vault, now)?;
    insert(
        vault,
        &path,
        &content,
        text(prefs, "heading"),
        prefs["prepend"] == true,
        true,
        &seed,
    )?;
    saved(ctx, vault, &path, prefs["silent"] == false).await
}

pub async fn task(
    ctx: &ExtensionContext,
    vault: &Vault,
    prefs: &Value,
    draft: &Value,
    now: DateTime<Local>,
) -> CoreResult {
    let raw = text(draft, "text");
    if raw.trim().is_empty() {
        return Err("Enter the task before saving".into());
    }
    let due = text(draft, "dueDate");
    markdown::ensure_due_date(due)?;
    let path = markdown::expand(ctx, text(prefs, "notePath"), "", now).await?;
    if path.trim().is_empty() {
        return Err("Set a Note Path in Append Task preferences".into());
    }
    let path = fs::note_path(&path, &vault.config, true)?;
    let expanded = markdown::expand(ctx, raw, text(prefs, "appendTemplate"), now).await?;
    let tag = prefs["noteTag"].as_str().unwrap_or("#task");
    let mut content = "- [ ] ".to_owned();
    if !tag.trim().is_empty() {
        content.push_str(tag);
        content.push(' ');
    }
    content.push_str(expanded.trim());
    if !due.is_empty() {
        content.push_str(&format!(" \u{1f4c5} {due}"));
    }
    if prefs["creationDate"] == true {
        content.push_str(&format!(" \u{2795} {}", now.format("%Y-%m-%d")));
    }
    insert(
        vault,
        &path,
        &content,
        text(prefs, "heading"),
        false,
        true,
        "",
    )?;
    saved(ctx, vault, &path, prefs["silent"] == false).await
}

pub async fn capture(
    ctx: &ExtensionContext,
    vault: &Vault,
    prefs: &Value,
    draft: &Value,
    now: DateTime<Local>,
) -> CoreResult {
    let (key, prefix) = match text(draft, "type") {
        "daily" => ("dailyNotePath", "- "),
        "todo" => ("todoNotePath", "- [ ] "),
        "shopping" => ("shoppingNotePath", "- "),
        _ => return Err("Choose a capture type".into()),
    };
    let path = text(prefs, key);
    if path.trim().is_empty() {
        return Err("Configure a note path for this capture type".into());
    }
    let path = fs::note_path(
        &markdown::expand(ctx, path, "", now).await?,
        &vault.config,
        false,
    )?;
    let body = markdown::expand(ctx, text(draft, "text"), "", now).await?;
    let body = body.trim();
    if body.is_empty() {
        return Err("Enter text before saving".into());
    }
    let split = body.lines().collect::<Vec<_>>().join("\n  ");
    let entry = format!("{prefix}{split}");
    insert(vault, &path, &entry, "", false, false, "")?;
    done(ctx, "Saved to Obsidian").await
}

pub fn preview_content(
    vault: &Vault,
    hit: &Hit,
    limit: usize,
) -> Result<(String, fs::Stamp), String> {
    let (raw, stamp) = vault
        .open()?
        .read(&hit.entry.rel, limit)
        .map_err(|e| e.to_string())?;
    if stamp.identity != hit.entry.stamp.identity {
        return Err("Selected file was replaced. Refresh the view.".into());
    }
    Ok((raw, stamp))
}

pub async fn run_action(
    ctx: &ExtensionContext,
    vault: &Vault,
    action: &CustomAction,
    draft: &Value,
    now: DateTime<Local>,
) -> CoreResult {
    action.validate()?;
    let raw = text(draft, "content");
    let first = raw
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .trim_start_matches('#')
        .trim();
    let first = crate::ui_text(first, 100)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join("_");
    let first = first.replace(['*', '_', '`'], "_");
    let input = if action.path.contains("{content}") {
        first.as_str()
    } else {
        ""
    };
    let path = markdown::expand(ctx, input, &action.path, now).await?;
    let path = fs::note_path(&path, &vault.config, true)?;
    let content = markdown::expand(
        ctx,
        raw,
        if action.kind == "template" {
            ""
        } else {
            &action.template
        },
        now,
    )
    .await?;
    insert(
        vault,
        &path,
        &content,
        &action.heading,
        action.mode == "prepend",
        true,
        "",
    )?;
    saved(ctx, vault, &path, !action.silent).await
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn percent_encoding_does_not_add_parameters() {
        assert_eq!(
            uri(
                "adv-uri",
                &[("vault", "A & B"), ("filepath", "Sub / note#1.md")]
            ),
            "obsidian://adv-uri?vault=A%20%26%20B&filepath=Sub%20%2F%20note%231.md"
        );
    }
}
