use crate::{
    fs::{self, Identity, Stamp, VaultDir},
    text,
};
use cap_fs_ext::DirExt;
use cap_std::fs::Dir;
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, HashSet},
    io::ErrorKind,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

const JSON_LIMIT: usize = 4 * 1024 * 1024;
const ENTRY_LIMIT: usize = 25_000;
const VISIT_LIMIT: usize = 100_000;

#[derive(Clone)]
pub enum Profile {
    Native,
    #[cfg(feature = "protocol-fixtures")]
    Fixture(PathBuf),
}

impl Profile {
    pub fn platform(&self, wire: &str) -> Result<bool, String> {
        match self {
            Self::Native => {
                let supported = match wire {
                    "x86_64-pc-windows-msvc" | "Windows" => cfg!(windows),
                    "aarch64-apple-darwin" | "x86_64-apple-darwin" | "macOS" => {
                        cfg!(target_os = "macos")
                    }
                    _ => false,
                };
                supported
                    .then_some(cfg!(windows))
                    .ok_or_else(|| "Obsidian extension supports macOS and Windows".into())
            }
            #[cfg(feature = "protocol-fixtures")]
            Self::Fixture(_) => match wire {
                "obsidian-fixture-windows" => Ok(true),
                "obsidian-fixture-macos" => Ok(false),
                _ => Err("Synthetic Obsidian fixture context required".into()),
            },
        }
    }

    pub fn fixture_root(&self) -> Option<&Path> {
        match self {
            Self::Native => None,
            #[cfg(feature = "protocol-fixtures")]
            Self::Fixture(path) => Some(path),
        }
    }

    fn registry(&self, windows: bool) -> Option<PathBuf> {
        if let Some(base) = self.fixture_root() {
            let branch = if windows {
                "Windows/obsidian.json"
            } else {
                "macOS/obsidian.json"
            };
            return Some(base.join(branch));
        }
        if windows {
            std::env::var_os("APPDATA")
                .map(PathBuf::from)
                .or_else(|| {
                    std::env::var_os("USERPROFILE")
                        .map(|home| PathBuf::from(home).join("AppData/Roaming"))
                })
                .map(|base| base.join("obsidian/obsidian.json"))
        } else {
            std::env::var_os("HOME").map(|home| {
                PathBuf::from(home).join("Library/Application Support/obsidian/obsidian.json")
            })
        }
    }

    pub fn discover(&self, windows: bool, prefs: &Value) -> Result<Vec<Vault>, String> {
        let config = fs::config_folder(text(prefs, "configFileName"))?;
        let explicit: Vec<_> = parse_list(text(prefs, "vaultPath"))
            .into_iter()
            .map(PathBuf::from)
            .collect();
        let registry = || {
            self.registry(windows)
                .and_then(|path| registry_data(&path, self.fixture_root()).ok())
                .unwrap_or_default()
        };
        let by_preference = !explicit.is_empty();
        let source = if by_preference {
            explicit.into_iter().map(|path| (None, path)).collect()
        } else {
            registry()
        };
        let mut configured = Vec::new();
        let mut vaults = Vec::new();
        let mut visited = HashSet::new();
        for (id, requested) in source.into_iter().take(64) {
            let Ok(path) = fs::checked_root(&requested, self.fixture_root()) else {
                continue;
            };
            if !visited.insert(path.clone()) {
                continue;
            }
            let Ok(dir) = VaultDir::open(&path, None) else {
                continue;
            };
            let Ok(identity) = dir.identity() else {
                continue;
            };
            let Some(name) = path.file_name().and_then(|v| v.to_str()) else {
                continue;
            };
            configured.push(requested);
            vaults.push(Vault {
                name: name.into(),
                path,
                id,
                identity,
                config: config.clone(),
            });
        }
        if by_preference && !vaults.is_empty() {
            let registered = registry();
            let compare = |path: &Path| {
                let value = fs::display_path(path)
                    .unwrap_or_default()
                    .replace('\\', "/");
                if windows {
                    value.to_lowercase()
                } else {
                    value
                }
            };
            let registered: Vec<_> = registered
                .into_iter()
                .filter_map(|(id, path)| id.map(|id| (id, compare(&path))))
                .collect();
            for (vault, requested) in vaults.iter_mut().zip(configured) {
                let canonical = compare(&vault.path);
                let requested = compare(&requested);
                vault.id = registered
                    .iter()
                    .find(|(_, path)| *path == canonical || *path == requested)
                    .map(|(id, _)| id.clone());
            }
        }
        Ok(vaults)
    }
}

fn registry_data(
    path: &Path,
    fixture: Option<&Path>,
) -> Result<Vec<(Option<String>, PathBuf)>, String> {
    let (parent, relative) = if let Some(root) = fixture {
        let relative = path
            .strip_prefix(root)
            .ok()
            .and_then(Path::to_str)
            .ok_or("Fixture registry is outside its profile")?;
        (root.to_path_buf(), relative.to_owned())
    } else {
        let parent = path
            .parent()
            .ok_or("Invalid Obsidian registry path")?
            .canonicalize()
            .map_err(|_| "Obsidian registry is unavailable")?;
        let filename = path
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or("Invalid Obsidian registry filename")?;
        (parent, filename.to_owned())
    };
    let dir = VaultDir::open(&parent, None).map_err(|_| "Obsidian registry is unavailable")?;
    let (content, _) = dir
        .read(&relative, JSON_LIMIT)
        .map_err(|_| "Obsidian registry could not be read")?;
    let data: Value =
        serde_json::from_str(&content).map_err(|_| "Obsidian registry is invalid JSON")?;
    let mut items: Vec<_> = data["vaults"]
        .as_object()
        .ok_or("Obsidian registry vaults are missing")?
        .iter()
        .filter_map(|(id, v)| {
            let path = v["path"].as_str()?;
            let id = (!id.is_empty()
                && id.len() <= 128
                && id.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '-'))
            .then(|| id.clone());
            Some((
                v["open"].as_bool().unwrap_or(false),
                v["ts"].as_u64().unwrap_or(0),
                id,
                PathBuf::from(path),
            ))
        })
        .collect();
    items.sort_by_key(|item| std::cmp::Reverse((item.0, item.1)));
    Ok(items
        .into_iter()
        .map(|(_, _, id, path)| (id, path))
        .collect())
}

pub fn parse_list(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .take(256)
        .map(str::to_owned)
        .collect()
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Vault {
    pub name: String,
    pub path: PathBuf,
    pub id: Option<String>,
    pub identity: Identity,
    pub config: String,
}

impl Vault {
    pub fn open(&self) -> Result<VaultDir, String> {
        VaultDir::open(&self.path, Some(self.identity)).map_err(|e| e.to_string())
    }
    pub fn key(&self) -> String {
        self.path.to_string_lossy().to_string()
    }
    pub fn display(&self) -> String {
        fs::display_path(&self.path).unwrap_or_else(|_| self.name.clone())
    }
    pub fn file_display(&self, path: &str) -> Result<String, String> {
        let path = fs::relative(path)?;
        fs::display_path(&self.path.join(path))
    }
    pub fn config_path(&self, file: &str) -> String {
        format!("{}/{file}", self.config)
    }
    pub fn json(&self, file: &str) -> Result<Option<(Value, Stamp)>, String> {
        let path = self.config_path(file);
        match self.open()?.read(&path, JSON_LIMIT) {
            Ok((raw, stamp)) => Ok(Some((
                serde_json::from_str(&raw).map_err(|_| format!("{file} is invalid JSON"))?,
                stamp,
            ))),
            Err(err) if err.kind() == ErrorKind::NotFound => Ok(None),
            Err(err) => Err(format!("Cannot read {file}: {err}")),
        }
    }
    fn config_value(&self, file: &str) -> Result<Value, String> {
        Ok(self
            .json(file)?
            .map(|(value, _)| value)
            .unwrap_or(Value::Null))
    }
    pub fn plugin(&self, id: &str) -> Result<bool, String> {
        let filename = if id == "obsidian-advanced-uri" {
            "community-plugins.json"
        } else {
            "core-plugins.json"
        };
        let config = self.config_value(filename)?;
        Ok(config[id] == true
            || config
                .as_array()
                .is_some_and(|list| list.iter().any(|v| v == id)))
    }
    pub fn daily_settings(&self) -> Result<Value, String> {
        self.config_value("daily-notes.json")
    }
    pub fn app_settings(&self) -> Result<Value, String> {
        self.config_value("app.json")
    }
    pub fn workspaces(&self) -> Result<BTreeMap<String, bool>, String> {
        let config = self.config_value("workspaces.json")?;
        Ok(config["workspaces"]
            .as_object()
            .into_iter()
            .flatten()
            .filter(|(key, _)| !key.trim().is_empty() && key.len() <= 512)
            .map(|(key, _)| (key.clone(), config["active"] == *key))
            .collect())
    }

    pub fn bookmarks(&self) -> Result<HashSet<String>, String> {
        let doc = self.config_value("bookmarks.json")?;
        let mut entries = HashSet::new();
        collect_bookmarks(&doc["items"], &mut entries, 0);
        Ok(entries)
    }

    pub fn toggle_bookmark(&self, entry: &Entry, bookmark: bool) -> Result<(), String> {
        let root = self.open()?;
        if root.stamp(&entry.rel).map_err(|e| e.to_string())?.identity != entry.stamp.identity {
            return Err("The note was replaced. Refresh before changing its bookmark.".into());
        }
        let (mut doc, stamp) = match self.json("bookmarks.json")? {
            Some((doc, stamp)) => (doc, Some(stamp)),
            None => (json!({"items":[]}), None),
        };
        let items = doc["items"]
            .as_array_mut()
            .ok_or("Bookmarks file does not contain an items list")?;
        let mut found = HashSet::new();
        if bookmark {
            collect_bookmarks(&Value::Array(items.clone()), &mut found, 0);
            if !found.contains(&entry.rel) {
                items.push(json!({"type":"file","path":entry.rel,"title":entry.title}));
            }
        } else {
            remove_bookmark(items, &entry.rel, 0);
        }
        let data = serde_json::to_vec_pretty(&doc).map_err(|_| "Cannot encode bookmarks")?;
        let path = self.config_path("bookmarks.json");
        match stamp {
            Some(stamp) => root.replace(&path, &data, stamp, JSON_LIMIT),
            None => root.create(&path, &data),
        }
        .map_err(|e| e.to_string())
    }
}

pub fn resolve_choice(vaults: &[Vault], value: &str) -> Result<usize, String> {
    let direct = vaults.iter().position(|vault| {
        vault.key() == value || vault.display() == value || vault.id.as_deref() == Some(value)
    });
    if let Some(index) = direct {
        return Ok(index);
    }
    let mut names = vaults
        .iter()
        .enumerate()
        .filter(|(_, vault)| vault.name.eq_ignore_ascii_case(value));
    let (index, _) = names
        .next()
        .ok_or("Configured Obsidian vault was not found")?;
    if names.next().is_some() {
        return Err("More than one vault has this name. Select a vault by its full path.".into());
    }
    Ok(index)
}

fn collect_bookmarks(items: &Value, target: &mut HashSet<String>, depth: usize) {
    if depth > 24 || target.len() >= ENTRY_LIMIT {
        return;
    }
    for item in items.as_array().into_iter().flatten().take(ENTRY_LIMIT) {
        match text(item, "type") {
            "file" => {
                if let Ok(path) = fs::relative(text(item, "path")) {
                    target.insert(path);
                }
            }
            "group" => collect_bookmarks(&item["items"], target, depth + 1),
            _ => {}
        }
    }
}
fn remove_bookmark(items: &mut Vec<Value>, searched: &str, depth: usize) {
    if depth > 24 {
        return;
    }
    items.retain(|item| {
        text(item, "type") != "file"
            || fs::relative(text(item, "path")).ok().as_deref() != Some(searched)
    });
    for item in items
        .iter_mut()
        .filter(|item| text(item, "type") == "group")
    {
        if let Some(children) = item["items"].as_array_mut() {
            remove_bookmark(children, searched, depth + 1);
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Entry {
    pub vault: usize,
    pub rel: String,
    pub title: String,
    pub kind: String,
    pub stamp: Stamp,
    pub starred: bool,
}

impl Entry {
    pub fn markdown(&self) -> bool {
        self.kind == "md"
    }
}

const MEDIA: &[&str] = &[
    "jpg", "png", "gif", "pdf", "webm", "mkv", "flv", "vob", "ogv", "ogg", "rrc", "gifv", "mng",
    "mov", "avi", "qt", "wmv", "yuv", "rm", "asf", "amv", "mp4", "m4p", "m4v", "mpg", "mp2",
    "mpeg", "mpe", "mpv", "svi", "3gp", "3g2", "mxf", "roq", "nsv", "f4v", "f4p", "f4a", "f4b",
    "mod", "aac", "aiff", "ape", "au", "flac", "gsm", "it", "m3u", "m4a", "mid", "mp3", "mpa",
    "pls", "ra", "s3m", "sid", "wav", "wma", "xm",
];

fn ignores(vault: &Vault, prefs: &Value) -> Result<GlobSet, String> {
    let mut patterns = parse_list(text(prefs, "excludedFolders"));
    let app = vault.app_settings()?;
    patterns.extend(
        app["userIgnoreFilters"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .take(256)
            .map(str::to_owned),
    );
    patterns.push(vault.config.clone());
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let pattern = pattern
            .trim()
            .replace('\\', "/")
            .trim_matches('/')
            .to_owned();
        if pattern.is_empty() {
            continue;
        }
        let expanded = if pattern.contains('/') {
            pattern
        } else {
            format!("**/{pattern}")
        };
        let glob = GlobBuilder::new(&expanded)
            .literal_separator(true)
            .case_insensitive(cfg!(windows))
            .build()
            .map_err(|_| "An Obsidian excluded-folder pattern is invalid")?;
        builder.add(glob);
    }
    builder
        .build()
        .map_err(|_| "Obsidian exclusion patterns exceed limits".into())
}

struct Walk<'a> {
    ignored: GlobSet,
    bookmarks: HashSet<String>,
    vault: usize,
    media: bool,
    visited: usize,
    result: Vec<Entry>,
    cancel: &'a AtomicBool,
    deadline: Instant,
    limited: bool,
}

impl Walk<'_> {
    fn directory(&mut self, dir: &Dir, prefix: &str, depth: usize) -> Result<(), String> {
        if depth > 32 {
            self.limited = true;
            return Ok(());
        }
        for item in dir
            .entries()
            .map_err(|_| "Could not enumerate selected vault")?
        {
            if self.cancel.load(Ordering::Relaxed) {
                return Err("Operation cancelled".into());
            }
            self.visited += 1;
            if self.result.len() >= ENTRY_LIMIT
                || self.visited > VISIT_LIMIT
                || Instant::now() >= self.deadline
            {
                self.limited = true;
                break;
            }
            let Ok(item) = item else {
                continue;
            };
            let Ok(name) = item.file_name().into_string() else {
                continue;
            };
            if name.starts_with('.') {
                continue;
            }
            let path = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            if self.ignored.is_match(&path) || fs::relative(&path).is_err() {
                continue;
            }
            let Ok(kind) = item.file_type() else {
                continue;
            };
            if kind.is_dir() {
                if let Ok(subdir) = dir.open_dir_nofollow(&name) {
                    self.directory(&subdir, &path, depth + 1)?;
                }
            } else if kind.is_file() {
                let extension = Path::new(&name)
                    .extension()
                    .and_then(|v| v.to_str())
                    .unwrap_or("")
                    .to_ascii_lowercase();
                if (self.media && !MEDIA.contains(&extension.as_str()))
                    || (!self.media && !matches!(extension.as_str(), "md" | "canvas"))
                {
                    continue;
                }
                let Ok(meta) = dir.symlink_metadata(&name) else {
                    continue;
                };
                if !meta.is_file() {
                    continue;
                }
                let title = if self.media {
                    name
                } else {
                    Path::new(&name)
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or(&name)
                        .to_owned()
                };
                self.result.push(Entry {
                    vault: self.vault,
                    starred: self.bookmarks.contains(&path),
                    rel: path,
                    title,
                    kind: extension,
                    stamp: Stamp::from(&meta),
                });
            }
        }
        Ok(())
    }
}

pub fn entries(
    vault: &Vault,
    index: usize,
    prefs: &Value,
    media: bool,
    cancel: &AtomicBool,
) -> Result<(Vec<Entry>, bool), String> {
    let root = vault.open()?;
    let bookmarks = if media {
        HashSet::new()
    } else {
        vault.bookmarks().unwrap_or_default()
    };
    let mut walk = Walk {
        ignored: ignores(vault, prefs)?,
        bookmarks,
        vault: index,
        media,
        visited: 0,
        result: Vec::new(),
        cancel,
        deadline: Instant::now() + Duration::from_secs(4),
        limited: false,
    };
    walk.directory(root.dir(), "", 0)?;
    walk.result.sort_by(|a, b| a.rel.cmp(&b.rel));
    Ok((walk.result, walk.limited))
}
