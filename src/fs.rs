//! Vault-relative file access. Every path component is opened without following links.
use cap_fs_ext::{DirExt, FollowSymlinks, MetadataExt, OpenOptionsFollowExt};
use cap_std::{
    ambient_authority,
    fs::{Dir, File, Metadata, OpenOptions},
};
use serde::{Deserialize, Serialize};
use std::{
    io::{self, ErrorKind, Read, Write},
    path::{Path, PathBuf},
};

pub const MAX_NOTE: usize = 8 * 1024 * 1024;
pub const MAX_DRAFT: usize = 512 * 1024;

fn invalid(reason: &str) -> io::Error {
    io::Error::new(ErrorKind::InvalidInput, reason)
}

fn valid_component(part: &str) -> bool {
    let stem = part.split('.').next().unwrap_or("").to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.as_bytes()[3].is_ascii_digit());
    !part.is_empty()
        && part != "."
        && part != ".."
        && !reserved
        && !part.ends_with(['.', ' '])
        && part.len() <= 255
        && !part
            .chars()
            .any(|ch| ch.is_control() || "<>:\"|?*".contains(ch))
}

pub fn relative(input: &str) -> Result<String, String> {
    let normalized = input.replace('\\', "/");
    if normalized.len() > 2048
        || normalized
            .split('/')
            .any(|component| !valid_component(component))
    {
        return Err("Use a relative path within the selected vault".into());
    }
    Ok(normalized)
}

pub fn note_path(input: &str, config: &str, add_extension: bool) -> Result<String, String> {
    let mut path = relative(input)?;
    if add_extension && !path.to_ascii_lowercase().ends_with(".md") {
        path.push_str(".md");
    }
    if !path.to_ascii_lowercase().ends_with(".md")
        || path
            .split('/')
            .any(|part| part.eq_ignore_ascii_case(config) || part.starts_with('.'))
    {
        return Err("Choose a visible Markdown (.md) path outside vault configuration".into());
    }
    Ok(path)
}

pub fn config_folder(input: &str) -> Result<String, String> {
    let folder = if input.trim().is_empty() {
        ".obsidian"
    } else {
        input.trim()
    };
    if folder.contains(['/', '\\'])
        || !valid_component(folder)
        || folder.eq_ignore_ascii_case(".trash")
    {
        return Err("Config filename must be a single folder name in the vault".into());
    }
    Ok(folder.into())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    dev: u64,
    inode: u64,
}
impl Identity {
    fn from(meta: &Metadata) -> Self {
        Self {
            dev: meta.dev(),
            inode: meta.ino(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stamp {
    pub identity: Identity,
    pub bytes: u64,
    pub modified: i64,
    pub created: i64,
    nanos: u32,
}

fn date(value: io::Result<cap_std::time::SystemTime>) -> (i64, u32) {
    value
        .ok()
        .map(|time| time.into_std())
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or((0, 0), |time| (time.as_secs() as i64, time.subsec_nanos()))
}

impl Stamp {
    pub fn from(meta: &Metadata) -> Self {
        let (modified, nanos) = date(meta.modified());
        Self {
            identity: Identity::from(meta),
            bytes: meta.len(),
            modified,
            created: date(meta.created()).0,
            nanos,
        }
    }
}

pub struct VaultDir {
    root: Dir,
}

impl VaultDir {
    pub fn open(path: &Path, identity: Option<Identity>) -> io::Result<Self> {
        if path.canonicalize()? != path {
            return Err(invalid("Selected vault path changed"));
        }
        let anchor = path
            .ancestors()
            .last()
            .ok_or_else(|| invalid("Vault has no filesystem root"))?;
        let mut root = Dir::open_ambient_dir(anchor, ambient_authority())?;
        let relative = path
            .strip_prefix(anchor)
            .map_err(|_| invalid("Vault path changed"))?;
        for component in relative.components() {
            let std::path::Component::Normal(name) = component else {
                return Err(invalid("Vault path is not canonical"));
            };
            root = root.open_dir_nofollow(name)?;
        }
        if identity.is_some_and(|expected| {
            root.dir_metadata().map(|meta| Identity::from(&meta)).ok() != Some(expected)
        }) {
            return Err(invalid("Selected vault was replaced. Reopen the command."));
        }
        Ok(Self { root })
    }

    pub fn identity(&self) -> io::Result<Identity> {
        Ok(Identity::from(&self.root.dir_metadata()?))
    }
    pub fn dir(&self) -> &Dir {
        &self.root
    }

    fn parent(&self, path: &str, create: bool) -> io::Result<(Dir, String)> {
        let validated = relative(path).map_err(|e| invalid(&e))?;
        let mut parts = validated.rsplitn(2, '/');
        let leaf = parts
            .next()
            .ok_or_else(|| invalid("Missing filename"))?
            .to_owned();
        let mut dir = self.root.try_clone()?;
        if let Some(parent) = parts.next() {
            for part in parent.split('/') {
                dir = match dir.open_dir_nofollow(part) {
                    Ok(opened) => opened,
                    Err(error) if create && error.kind() == ErrorKind::NotFound => {
                        match dir.create_dir(part) {
                            Ok(()) => {}
                            Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
                            Err(error) => return Err(error),
                        }
                        dir.open_dir_nofollow(part)?
                    }
                    Err(error) => return Err(error),
                };
            }
        }
        Ok((dir, leaf))
    }

    pub fn stamp(&self, path: &str) -> io::Result<Stamp> {
        let (parent, leaf) = self.parent(path, false)?;
        stamp_at(&parent, &leaf)
    }

    pub fn read(&self, path: &str, limit: usize) -> io::Result<(String, Stamp)> {
        let (parent, leaf) = self.parent(path, false)?;
        read_at(&parent, &leaf, limit)
    }

    pub fn create(&self, path: &str, data: &[u8]) -> io::Result<()> {
        let (parent, leaf) = self.parent(path, true)?;
        create_at(&parent, &leaf, data)
    }

    pub fn append(&self, path: &str, content: &str, expected: Option<Identity>) -> io::Result<()> {
        let (parent, leaf) = self.parent(path, false)?;
        let mut opts = OpenOptions::new();
        opts.read(true).append(true).follow(FollowSymlinks::No);
        let file = open_regular(&parent, &leaf, &opts, true)?;
        let meta = file.metadata()?;
        if expected.is_some_and(|id| id != Identity::from(&meta)) {
            return Err(invalid(
                "Selected file was replaced. Refresh before saving.",
            ));
        }
        let mut file = file.into_std();
        let mut bytes = Vec::new();
        if meta.len() > 0 {
            bytes.push(b'\n');
        }
        bytes.extend_from_slice(content.as_bytes());
        file.write_all(&bytes)
            .and_then(|_| file.sync_data())
            .map_err(|error| {
                io::Error::new(
                    error.kind(),
                    "Append could not be confirmed. Check the note before retrying.",
                )
            })
    }

    pub fn replace(
        &self,
        path: &str,
        data: &[u8],
        original: Stamp,
        limit: usize,
    ) -> io::Result<()> {
        let (parent, leaf) = self.parent(path, false)?;
        if original.bytes > limit as u64 || data.len() > limit {
            return Err(invalid("File exceeds the safe edit limit"));
        }
        let temp = format!(".tezbar-{}.tmp", uuid::Uuid::new_v4());
        if let Err(error) = create_at(&parent, &temp, data) {
            return Err(error);
        }
        let result = (|| {
            let meta = parent.symlink_metadata(&leaf)?;
            if !meta.is_file() || meta.nlink() != 1 || Stamp::from(&meta) != original {
                return Err(invalid(
                    "The file changed while it was open. Copy your draft, then reopen the note.",
                ));
            }
            parent.set_permissions(&temp, meta.permissions())?;
            // Use a same-directory atomic replace. The preflight avoids replacing stale content.
            if stamp_at(&parent, &leaf)? != original {
                return Err(invalid("File changed immediately before saving"));
            }
            parent.rename(&temp, &parent, &leaf)
        })();
        if result.is_err() {
            let _ = parent.remove_file(&temp);
        }
        result
    }

    pub fn trash(&self, path: &str, expected: Stamp) -> io::Result<String> {
        let (parent, leaf) = self.parent(path, false)?;
        let trash_name = format!("{}-{leaf}", uuid::Uuid::new_v4());
        let (trash, leaf_out) = self.parent(&format!(".trash/{trash_name}"), true)?;
        if stamp_at(&parent, &leaf)? != expected {
            return Err(invalid("Selected file changed. Refresh and confirm again."));
        }
        parent.rename(leaf, &trash, leaf_out)?;
        Ok(format!(".trash/{trash_name}"))
    }
}

fn stamp_at(dir: &Dir, leaf: &str) -> io::Result<Stamp> {
    let meta = dir.symlink_metadata(leaf)?;
    if !meta.is_file() {
        return Err(invalid("Obsidian paths must resolve to regular files"));
    }
    Ok(Stamp::from(&meta))
}

fn open_regular(dir: &Dir, leaf: &str, opts: &OpenOptions, write: bool) -> io::Result<File> {
    let before = stamp_at(dir, leaf)?;
    let file = dir.open_with(leaf, opts)?;
    let meta = file.metadata()?;
    if !meta.is_file() || before.identity != Identity::from(&meta) || (write && meta.nlink() != 1) {
        return Err(invalid("Selected file is linked or changed while opening"));
    }
    Ok(file)
}

fn read_at(dir: &Dir, leaf: &str, limit: usize) -> io::Result<(String, Stamp)> {
    let mut opts = OpenOptions::new();
    opts.read(true).follow(FollowSymlinks::No);
    let mut file = open_regular(dir, leaf, &opts, false)?;
    let before = Stamp::from(&file.metadata()?);
    if before.bytes > limit as u64 {
        return Err(invalid("File exceeds the supported read limit"));
    }
    let mut bytes = Vec::new();
    (&mut file).take(limit as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > limit
        || before != Stamp::from(&file.metadata()?)
        || before.bytes != bytes.len() as u64
    {
        return Err(invalid("File changed or exceeded the read limit"));
    }
    Ok((
        String::from_utf8(bytes).map_err(|_| invalid("Obsidian file is not UTF-8 text"))?,
        before,
    ))
}

fn create_at(dir: &Dir, leaf: &str, data: &[u8]) -> io::Result<()> {
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true).follow(FollowSymlinks::No);
    let mut file = dir.open_with(leaf, &opts)?;
    let id = Identity::from(&file.metadata()?);
    let result = file.write_all(data).and_then(|_| file.sync_all());
    drop(file);
    if result.is_err() && stamp_at(dir, leaf).is_ok_and(|stamp| stamp.identity == id) {
        let _ = dir.remove_file(leaf);
    }
    result
}

pub fn display_path(path: &Path) -> Result<String, String> {
    let input = path.to_str().ok_or("Obsidian path is not Unicode")?;
    #[cfg(windows)]
    {
        if let Some(unc) = input.strip_prefix(r"\\?\UNC\") {
            return Ok(format!(r"\\{unc}"));
        }
        if let Some(local) = input.strip_prefix(r"\\?\") {
            return Ok(local.to_owned());
        }
    }
    Ok(input.into())
}

pub fn checked_root(candidate: &Path, fixture: Option<&Path>) -> Result<PathBuf, String> {
    if !candidate.is_absolute() {
        return Err("Obsidian vault path must be absolute".into());
    }
    let path = candidate
        .canonicalize()
        .map_err(|_| "Obsidian vault folder is unavailable")?;
    if !path.is_dir() || fixture.is_some_and(|base| !path.starts_with(base) || path == base) {
        return Err("The selected vault is outside the available profile".into());
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn safe_writes_and_relative_paths() {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().canonicalize().unwrap();
        let root = VaultDir::open(&base, None).unwrap();
        for invalid in [
            "../outside.md",
            "C:\\elsewhere.md",
            "x/file.md:stream",
            "/global.md",
            "folder/../x.md",
            "CON.txt",
        ] {
            assert!(relative(invalid).is_err(), "{invalid}");
        }
        root.create("folder/test.md", b"original").unwrap();
        assert_eq!(
            root.create("folder/test.md", b"replacement")
                .unwrap_err()
                .kind(),
            ErrorKind::AlreadyExists
        );
        let stamp = root.stamp("folder/test.md").unwrap();
        root.append("folder/test.md", "new", Some(stamp.identity))
            .unwrap();
        assert!(root
            .replace("folder/test.md", b"invalid edit", stamp, MAX_NOTE)
            .is_err());
        assert_eq!(
            root.read("folder/test.md", MAX_NOTE).unwrap().0,
            "original\nnew"
        );
        assert!(root.read("folder/test.md", 3).is_err());
        let same = root.stamp("folder/test.md").unwrap();
        root.replace("folder/test.md", b"updated", same, MAX_NOTE)
            .unwrap();
        let new = root.stamp("folder/test.md").unwrap();
        let trash = root.trash("folder/test.md", new).unwrap();
        assert_eq!(root.read(&trash, MAX_NOTE).unwrap().0, "updated");
    }

    #[test]
    fn hard_link_append_preserves_outside_data() {
        let outer = tempfile::tempdir().unwrap();
        let external = outer.path().join("outside.md");
        let vault = outer.path().join("vault");
        std::fs::create_dir(&vault).unwrap();
        std::fs::write(&external, "preserve").unwrap();
        std::fs::hard_link(&external, vault.join("linked.md")).unwrap();
        let root = VaultDir::open(&vault.canonicalize().unwrap(), None).unwrap();
        assert!(root.append("linked.md", "must not change", None).is_err());
        assert_eq!(std::fs::read_to_string(external).unwrap(), "preserve");
    }

    #[cfg(unix)]
    #[test]
    fn refuses_link_components() {
        let outer = tempfile::tempdir().unwrap();
        let vault = outer.path().join("vault");
        std::fs::create_dir(&vault).unwrap();
        std::os::unix::fs::symlink(outer.path(), vault.join("escape")).unwrap();
        let root = VaultDir::open(&vault.canonicalize().unwrap(), None).unwrap();
        assert!(root.create("escape/unexpected.md", b"escape").is_err());
        assert!(!outer.path().join("unexpected.md").exists());
    }
}
