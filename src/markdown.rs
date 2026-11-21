use crate::{
    fs::{self, MAX_DRAFT, MAX_NOTE},
    text,
    vault::{Entry, Vault},
};
use chrono::{DateTime, Datelike, Local, NaiveDate};
use regex::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::BTreeSet,
    io::ErrorKind,
    sync::{
        atomic::{AtomicBool, Ordering},
        OnceLock,
    },
    time::{Duration, Instant},
};
use tezbar_extension_sdk::ExtensionContext;

const CONTENT_LIMIT: usize = 1024 * 1024;
const SEARCH_BYTES: u64 = 64 * 1024 * 1024;
pub const RESULT_LIMIT: usize = 1000;

fn pattern(cache: &'static OnceLock<Regex>, value: &str) -> &'static Regex {
    cache.get_or_init(|| Regex::new(value).expect("valid regex"))
}

fn frontmatter(input: &str) -> Option<(&str, &str)> {
    let rest = input
        .strip_prefix("---\r\n")
        .or_else(|| input.strip_prefix("---\n"))?;
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        if matches!(line.trim_end_matches(['\r', '\n']), "---" | "...") {
            return Some((&rest[..offset], &rest[offset + line.len()..]));
        }
        offset += line.len();
        if offset > 64 * 1024 {
            break;
        }
    }
    None
}

pub fn tags(content: &str) -> Vec<String> {
    static CODE: OnceLock<Regex> = OnceLock::new();
    static TAG: OnceLock<Regex> = OnceLock::new();
    let mut tags = BTreeSet::new();
    let raw = if let Some((yaml, body)) = frontmatter(content) {
        if let Ok(value) = serde_yaml::from_str::<serde_yaml::Value>(yaml) {
            for field in ["tag", "tags"] {
                if let Some(values) = value[field].as_sequence() {
                    for tag in values
                        .iter()
                        .take(256)
                        .filter_map(serde_yaml::Value::as_str)
                    {
                        tags.insert(tag.trim_start_matches('#').to_owned());
                    }
                } else if let Some(values) = value[field].as_str() {
                    for tag in values
                        .split(|ch: char| ch.is_whitespace() || ch == ',')
                        .filter(|tag| !tag.is_empty())
                        .take(256)
                    {
                        tags.insert(tag.trim_start_matches('#').to_owned());
                    }
                }
            }
        }
        body
    } else {
        content
    };
    let raw = pattern(
        &CODE,
        r"(?ms)^\s*(?:```|~~~)[^\n]*\n.*?^\s*(?:```|~~~)[^\n]*",
    )
    .replace_all(raw, "");
    let matcher = pattern(&TAG, r"(?:^|[\s(])#([\p{L}\p{N}_/-]+)");
    for found in matcher.captures_iter(&raw).take(1024) {
        tags.insert(found[1].to_owned());
    }
    tags.into_iter()
        .filter(|tag| !tag.is_empty())
        .take(256)
        .collect()
}

pub fn frontmatter_for(tags: &[String]) -> Result<String, String> {
    let tags: Vec<_> = tags
        .iter()
        .take(128)
        .map(|tag| tag.trim().trim_start_matches('#'))
        .filter(|tag| !tag.is_empty())
        .collect();
    if tags.is_empty() {
        return Ok(String::new());
    }
    if tags
        .iter()
        .any(|tag| tag.len() > 100 || tag.chars().any(char::is_control))
    {
        return Err("Invalid Obsidian tag".into());
    }
    let list =
        serde_yaml::to_string(&json!({"tags":tags})).map_err(|_| "Cannot encode note tags")?;
    Ok(format!("---\n{list}---\n\n"))
}

pub fn clean(content: &str, prefs: &Value) -> String {
    static LATEX: OnceLock<Regex> = OnceLock::new();
    static LINKS: OnceLock<Regex> = OnceLock::new();
    static WIKI: OnceLock<Regex> = OnceLock::new();
    let trimmed = if prefs["removeYAML"] == true {
        frontmatter(content).map_or(content, |(_, body)| body)
    } else {
        content
    };
    let mut text = trimmed.to_owned();
    if prefs["removeLatex"] == true {
        text = pattern(&LATEX, r"(?s)\$\$.*?\$\$|\$[^$\n]+\$")
            .replace_all(&text, "")
            .into_owned();
    }
    if prefs["removeLinks"] == true {
        text = pattern(&LINKS, r"!?\[([^\]\n]+)\]\([^\)]+\)")
            .replace_all(&text, "$1")
            .into_owned();
        text = pattern(&WIKI, r"!?\[\[(?:[^\]|]+\|)?([^\]\n]+)\]\]")
            .replace_all(&text, "$1")
            .into_owned();
    }
    text
}

pub fn code_blocks(value: &str) -> Vec<(String, String)> {
    static CODE: OnceLock<Regex> = OnceLock::new();
    pattern(&CODE, r"(?ms)^```([^\r\n]*)\r?\n(.*?)^```\s*$")
        .captures_iter(value)
        .take(50)
        .map(|found| (found[1].to_owned(), found[2].to_owned()))
        .collect()
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Hit {
    pub entry: Entry,
    pub line: Option<usize>,
    pub column: Option<usize>,
    pub context: String,
}

fn fuzzy(query: &str, text: &str) -> bool {
    if text.contains(query) {
        return true;
    }
    if query.chars().count() < 3 {
        return false;
    }
    let mut chars = query.chars();
    let mut next = chars.next();
    let mut first = None;
    for (index, ch) in text.chars().take(2200).enumerate() {
        if Some(ch) == next {
            if first.is_none() {
                first = Some(index);
            }
            next = chars.next();
            if next.is_none() {
                return index + 1 - first.unwrap_or(0) <= query.chars().count() * 4 / 3;
            }
        }
    }
    false
}

pub fn search(
    vault: &Vault,
    entries: &[Entry],
    query: &str,
    prefs: &Value,
    starred: bool,
    kind: &str,
    sort: &str,
    cancel: &AtomicBool,
) -> Result<(Vec<Hit>, bool), String> {
    if query.len() > 1024 {
        return Err("Obsidian search is limited to 1024 bytes".into());
    }
    static TAG: OnceLock<Regex> = OnceLock::new();
    let matcher = pattern(&TAG, r"(?i)(?:^|\s)tag:(\S+)");
    let wanted: Vec<_> = matcher
        .captures_iter(query)
        .take(16)
        .map(|capture| capture[1].trim_start_matches('#').to_lowercase())
        .collect();
    let needle = matcher.replace_all(query, " ").trim().to_owned();
    let words: Vec<_> = needle
        .split_whitespace()
        .take(40)
        .map(str::to_lowercase)
        .collect();
    let content_search = prefs["searchContent"] == true;
    let fuzzy_search = prefs["fuzzySearch"] == true || content_search;
    let regex = if content_search && !needle.is_empty() {
        Some(
            RegexBuilder::new(&regex::escape(&needle))
                .case_insensitive(true)
                .build()
                .map_err(|_| "Invalid literal search")?,
        )
    } else {
        None
    };
    let mut hits = Vec::new();
    let mut scanned = 0;
    let mut limited = false;
    let deadline = Instant::now() + Duration::from_secs(4);
    let dir = vault.open()?;
    for entry in entries {
        if cancel.load(Ordering::Relaxed) {
            return Err("Operation cancelled".into());
        }
        if hits.len() >= RESULT_LIMIT || Instant::now() >= deadline {
            limited = true;
            break;
        }
        if starred && !entry.starred || (!kind.is_empty() && kind != "all" && kind != entry.kind) {
            continue;
        }
        let title = entry.title.to_lowercase();
        let path = entry.rel.to_lowercase();
        let title_match = words.iter().all(|word| {
            if fuzzy_search {
                fuzzy(word, &title) || fuzzy(word, &path)
            } else {
                title.contains(word) || path.contains(word)
            }
        });
        let read = entry.markdown()
            && (!wanted.is_empty() || regex.is_some())
            && entry.stamp.bytes <= CONTENT_LIMIT as u64;
        let content = if read && scanned + entry.stamp.bytes <= SEARCH_BYTES {
            scanned += entry.stamp.bytes;
            dir.read(&entry.rel, CONTENT_LIMIT)
                .ok()
                .map(|(value, _)| value)
        } else {
            if read {
                limited = true;
            }
            None
        };
        if !wanted.is_empty() {
            let Some(content) = &content else {
                continue;
            };
            let tags: Vec<_> = tags(content)
                .into_iter()
                .map(|tag| tag.to_lowercase())
                .collect();
            if !wanted.iter().all(|tag| {
                tags.iter()
                    .any(|value| value == tag || value.starts_with(&format!("{tag}/")))
            }) {
                continue;
            }
        }
        if title_match {
            hits.push(Hit {
                entry: entry.clone(),
                line: None,
                column: None,
                context: String::new(),
            });
        }
        if let (Some(regex), Some(content)) = (&regex, &content) {
            let mut offset = 0;
            let mut count = 0;
            for (line_no, line) in content.split_inclusive('\n').enumerate() {
                for occurrence in regex.find_iter(line).take(12) {
                    let from = occurrence.start();
                    let column = line[..from].chars().count() + 1;
                    hits.push(Hit {
                        entry: entry.clone(),
                        line: Some(line_no + 1),
                        column: Some(column),
                        context: crate::ui_text(line.trim(), 420),
                    });
                    count += 1;
                    if count >= 12 || hits.len() >= RESULT_LIMIT {
                        break;
                    }
                }
                offset += line.len();
                if count >= 12 || hits.len() >= RESULT_LIMIT || offset >= CONTENT_LIMIT {
                    break;
                }
            }
        }
    }
    match sort {
        "alphabetical-asc" => hits.sort_by(|a, b| {
            a.entry
                .title
                .to_lowercase()
                .cmp(&b.entry.title.to_lowercase())
        }),
        "alphabetical-desc" => hits.sort_by(|a, b| {
            b.entry
                .title
                .to_lowercase()
                .cmp(&a.entry.title.to_lowercase())
        }),
        "modified-desc" => hits.sort_by_key(|hit| std::cmp::Reverse(hit.entry.stamp.modified)),
        "modified-asc" => hits.sort_by_key(|hit| hit.entry.stamp.modified),
        "created-desc" => hits.sort_by_key(|hit| std::cmp::Reverse(hit.entry.stamp.created)),
        "created-asc" => hits.sort_by_key(|hit| hit.entry.stamp.created),
        _ => hits.sort_by_key(|hit| hit.line.is_some()),
    }
    Ok((hits, limited))
}

fn format_token(now: DateTime<Local>, tag: &str) -> Option<String> {
    let fmt = match tag {
        "date" => "%Y-%m-%d",
        "time" => "%H:%M:%S",
        "week" | "WW" => "%V",
        "W" => "%-V",
        "year" | "yyyy" | "y" => "%Y",
        "month" | "MMMM" | "LLLL" => "%B",
        "day" | "cccc" | "EEEE" => "%A",
        "hour" | "HH" => "%H",
        "minute" | "mm" => "%M",
        "second" | "ss" => "%S",
        "millisecond" | "SSS" => "%3f",
        "MMM" | "LLL" => "%b",
        "ccc" | "EEE" => "%a",
        "yy" => "%y",
        "MM" | "LL" => "%m",
        "M" | "L" => "%-m",
        "dd" => "%d",
        "d" => "%-d",
        "H" => "%-H",
        "hh" => "%I",
        "h" => "%-I",
        "m" => "%-M",
        "s" => "%-S",
        "a" => "%p",
        "kk" => "%g",
        "kkkk" => "%G",
        "ooo" => "%j",
        "o" => "%-j",
        "ZZ" => "%:z",
        "ZZZ" => "%z",
        "ZZZZ" | "ZZZZZ" | "z" => "%Z",
        "X" => "%s",
        "timestamp" | "x" | "zettelkastenID" => return Some(now.timestamp_millis().to_string()),
        "q" => return Some(((now.month0() / 3) + 1).to_string()),
        "qq" => return Some(format!("{:02}", (now.month0() / 3) + 1)),
        "S" => return Some(now.timestamp_subsec_millis().to_string()),
        "u" => "%3f",
        "uu" => "%6f",
        "uuu" => "%9f",
        "newline" | "nl" => return Some("\n".into()),
        _ => return None,
    };
    Some(now.format(fmt).to_string())
}

pub async fn expand(
    ctx: &ExtensionContext,
    content: &str,
    template: &str,
    now: DateTime<Local>,
) -> Result<String, String> {
    if content.len() + template.len() > MAX_DRAFT {
        return Err("Obsidian draft exceeds 512 KiB".into());
    }
    let placeholders = template.matches("{content}").count();
    let expected =
        template.len() - placeholders * "{content}".len() + placeholders.max(1) * content.len();
    if expected > MAX_DRAFT {
        return Err("Expanded Obsidian draft exceeds 512 KiB".into());
    }
    let joined = if placeholders > 0 {
        template.replace("{content}", content)
    } else {
        format!("{template}{content}")
    };
    if joined.len() > MAX_DRAFT {
        return Err("Expanded Obsidian draft exceeds 512 KiB".into());
    }
    static VAR: OnceLock<Regex> = OnceLock::new();
    let matcher = pattern(&VAR, r"\{([a-zA-Z][a-zA-Z0-9]*)\}");
    let mut clipboard = None;
    let mut selection = None;
    let mut output = String::new();
    let mut pos = 0;
    for found in matcher.captures_iter(&joined) {
        let full = found.get(0).expect("template capture");
        output.push_str(&joined[pos..full.start()]);
        let value = match &found[1] {
            "clipboard" | "clip" => {
                if clipboard.is_none() {
                    clipboard = Some(
                        ctx.host("host.clipboard.read", json!({"format":"text"}))
                            .await?
                            .as_str()
                            .unwrap_or("")
                            .to_owned(),
                    );
                }
                clipboard.clone()
            }
            "selection" | "selected" => {
                if selection.is_none() {
                    selection = Some(
                        ctx.host("host.selection.read", json!({"kind":"text"}))
                            .await?
                            .as_str()
                            .unwrap_or("")
                            .to_owned(),
                    );
                }
                selection.clone()
            }
            token => format_token(now, token),
        };
        output.push_str(value.as_deref().unwrap_or(full.as_str()));
        pos = full.end();
        if output.len() > MAX_DRAFT {
            return Err("Expanded Obsidian draft exceeds 512 KiB".into());
        }
    }
    output.push_str(&joined[pos..]);
    if output.len() > MAX_DRAFT {
        return Err("Expanded Obsidian draft exceeds 512 KiB".into());
    }
    Ok(output)
}

pub fn moment(format: &str, now: DateTime<Local>) -> Result<String, String> {
    if format.len() > 128 {
        return Err("Obsidian date format is too long".into());
    }
    let mut output = String::new();
    let mut rest = format;
    let tokens = [
        ("YYYY", "%Y"),
        ("GGGG", "%G"),
        ("MMMM", "%B"),
        ("dddd", "%A"),
        ("MMM", "%b"),
        ("ddd", "%a"),
        ("SSS", "%3f"),
        ("YY", "%y"),
        ("GG", "%g"),
        ("MM", "%m"),
        ("DD", "%d"),
        ("HH", "%H"),
        ("hh", "%I"),
        ("mm", "%M"),
        ("ss", "%S"),
        ("WW", "%V"),
        ("ZZ", "%z"),
        ("M", "%-m"),
        ("D", "%-d"),
        ("H", "%-H"),
        ("h", "%-I"),
        ("m", "%-M"),
        ("s", "%-S"),
        ("W", "%-V"),
        ("A", "%p"),
        ("a", "%P"),
        ("Z", "%:z"),
        ("X", "%s"),
    ];
    while !rest.is_empty() {
        if let Some(literal) = rest.strip_prefix('[') {
            let close = literal
                .find(']')
                .ok_or("Unclosed literal in Obsidian date format")?;
            output.push_str(&literal[..close]);
            rest = &literal[close + 1..];
        } else if let Some(next) = rest.strip_prefix("Do") {
            let n = now.day();
            let suffix = if (11..=13).contains(&(n % 100)) {
                "th"
            } else {
                match n % 10 {
                    1 => "st",
                    2 => "nd",
                    3 => "rd",
                    _ => "th",
                }
            };
            output.push_str(&format!("{n}{suffix}"));
            rest = next;
        } else if let Some((token, fmt)) = tokens.iter().find(|(token, _)| rest.starts_with(token))
        {
            output.push_str(&now.format(fmt).to_string());
            rest = &rest[token.len()..];
        } else {
            let ch = rest.chars().next().expect("remaining character");
            output.push(ch);
            rest = &rest[ch.len_utf8()..];
        }
    }
    Ok(output)
}

pub fn ensure_due_date(value: &str) -> Result<(), String> {
    if value.is_empty()
        || (value.len() == 10 && NaiveDate::parse_from_str(value, "%Y-%m-%d").is_ok())
    {
        Ok(())
    } else {
        Err("Enter the due date as YYYY-MM-DD".into())
    }
}

pub fn daily_target(vault: &Vault, now: DateTime<Local>) -> Result<(String, String), String> {
    if !vault.plugin("daily-notes")? {
        return Err("Enable Daily Notes in the selected vault".into());
    }
    let settings = vault.daily_settings()?;
    let fmt = settings["format"]
        .as_str()
        .filter(|value| !value.is_empty())
        .unwrap_or("YYYY-MM-DD");
    let name = moment(fmt, now)?;
    let folder = text(&settings, "folder").trim_end_matches(['/', '\\']);
    let rel = fs::note_path(
        &if folder.is_empty() {
            name
        } else {
            format!("{folder}/{name}")
        },
        &vault.config,
        true,
    )?;
    let template = text(&settings, "template");
    let mut seed = String::new();
    let root = vault.open()?;
    match root.stamp(&rel) {
        Ok(_) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => {
            if !template.is_empty() {
                let path = fs::note_path(template, &vault.config, true)?;
                seed = root
                    .read(&path, MAX_DRAFT)
                    .map_err(|e| format!("Cannot read Daily Note template: {e}"))?
                    .0;
                static DATE: OnceLock<Regex> = OnceLock::new();
                let matcher = pattern(&DATE, r"\{\{\s*(date|time)(?::([^}]+))?\s*\}\}");
                seed = matcher
                    .replace_all(&seed, |capture: &regex::Captures| {
                        let fmt = capture
                            .get(2)
                            .map(|value| value.as_str())
                            .unwrap_or(if &capture[1] == "date" { fmt } else { "HH:mm" });
                        moment(fmt.trim(), now).unwrap_or_else(|_| capture[0].to_owned())
                    })
                    .into_owned();
            }
        }
        Err(error) => return Err(error.to_string()),
    }
    Ok((rel, seed))
}

pub fn inserted(
    original: &str,
    content: &str,
    heading: &str,
    prepend: bool,
) -> Result<String, String> {
    if original.len() + content.len() + 4 > MAX_NOTE {
        return Err("The note is too large for a heading or prepend edit".into());
    }
    let split = if heading.trim().is_empty() {
        if prepend {
            frontmatter(original).map_or(0, |(_, body)| original.len() - body.len())
        } else {
            original.len()
        }
    } else {
        let mut offset = 0;
        let mut open: Option<(usize, usize)> = None;
        let mut end = original.len();
        let mut fenced = false;
        for line in original.split_inclusive('\n') {
            let body = line.trim();
            if body.starts_with("```") || body.starts_with("~~~") {
                fenced = !fenced;
            }
            let level = body.bytes().take_while(|byte| *byte == b'#').count();
            if !fenced && (1..=6).contains(&level) && body[level..].starts_with(' ') {
                if let Some((current, _)) = open {
                    if level <= current {
                        end = offset;
                        break;
                    }
                } else if body[level..].trim().trim_end_matches('#').trim()
                    == heading.trim().trim_start_matches('#').trim()
                {
                    open = Some((level, offset + line.len()));
                }
            }
            offset += line.len();
        }
        let (_, begin) = open.ok_or("Heading was not found in the selected note")?;
        if prepend {
            begin
        } else {
            end
        }
    };
    let (before, after) = original.split_at(split);
    let line_end = if original.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let before_sep = if before.is_empty() || before.ends_with('\n') {
        ""
    } else {
        line_end
    };
    let after_sep = if after.is_empty() || content.ends_with('\n') {
        ""
    } else {
        line_end
    };
    Ok(format!("{before}{before_sep}{content}{after_sep}{after}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    #[test]
    fn tags_formats_and_insertion_preserve_markdown() {
        let note="---\ntags: [one, two]\n---\n# Log\nPrior\n## Child\nContent\n# Other\n\n#inline\n```css\n #ignore\n```\n";
        assert_eq!(tags(note), vec!["inline", "one", "two"]);
        assert!(inserted(note, "New", "Log", false)
            .unwrap()
            .contains("Content\nNew\n# Other"));
        assert!(inserted(note, "New", "", true)
            .unwrap()
            .contains("---\nNew\n# Log"));
        assert!(inserted(note, "New", "Absent", false).is_err());
        assert!(ensure_due_date("2026-02-31").is_err());
        let now = Local
            .with_ymd_and_hms(2026, 11, 21, 14, 30, 0)
            .single()
            .unwrap();
        assert_eq!(
            moment("YYYY/[Week] WW/YYYY-MM-DD", now).unwrap(),
            "2026/Week 47/2026-11-21"
        );
    }
}
