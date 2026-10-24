//! Build-time source packaging. Never used to read files from an HTTP request.
use flate2::{Compression, GzBuilder};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Component, Path};
use std::process::Command;

const MANIFEST: &str = "SOURCE-MANIFEST.json";
const PREFIX: &str = "outpace-acestream";

#[derive(Serialize, Deserialize)]
struct Manifest {
    format: u32,
    base_revision: String,
    tree_state: String,
    files: Vec<Entry>,
}

#[derive(Serialize, Deserialize)]
struct Entry {
    path: String,
    mode: u32,
    sha256: String,
}

pub struct Bundle {
    pub bytes: Vec<u8>,
    pub paths: Vec<String>,
    pub revision: String,
    pub tree_state: String,
    pub sha256: String,
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn git(root: &Path, args: &[&str]) -> io::Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()?;
    if !output.status.success() {
        return Err(invalid("git source inventory failed"));
    }
    String::from_utf8(output.stdout).map_err(|_| invalid("source paths must be UTF-8"))
}

pub fn git_path(root: &Path, name: &str) -> io::Result<String> {
    git(
        root,
        &["rev-parse", "--path-format=absolute", "--git-path", name],
    )
    .map(|value| value.trim().to_owned())
}

fn validate_path(path: &str) -> io::Result<()> {
    if path.is_empty()
        || path.contains(['\\', '\n', '\r'])
        || !Path::new(path)
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
        || path.split('/').any(|part| {
            matches!(
                part,
                ".git" | "target" | "re" | "references" | "acestream-ids.txt" | ".env"
            )
        })
        || path == MANIFEST
    {
        return Err(invalid(format!("unsafe source path: {path}")));
    }
    Ok(())
}

fn read_entry(root: &Path, entry: &Entry) -> io::Result<Vec<u8>> {
    validate_path(&entry.path)?;
    let path = root.join(&entry.path);
    let mut parent = path.parent();
    while let Some(directory) = parent {
        if directory == root {
            break;
        }
        if fs::symlink_metadata(directory)?.file_type().is_symlink() {
            return Err(invalid("source parent directory is a symlink"));
        }
        parent = directory.parent();
    }
    let info = fs::symlink_metadata(&path)?;
    match entry.mode {
        0o100644 | 0o100755 if info.is_file() => fs::read(path),
        0o120000 => {
            // Windows checkouts with core.symlinks=false store the link target
            // as a regular file. Preserve the indexed link, never dereference it.
            let target = if info.file_type().is_symlink() {
                fs::read_link(path)?
                    .to_str()
                    .ok_or_else(|| invalid("non-UTF8 symlink"))?
                    .to_owned()
            } else if info.is_file() {
                fs::read_to_string(path)?
            } else {
                return Err(invalid("invalid source symlink"));
            };
            validate_path(&target)?;
            Ok(target.into_bytes())
        }
        _ => Err(invalid(
            "unsupported source file type (or regular file replaced by a symlink)",
        )),
    }
}

fn inventory(root: &Path) -> io::Result<Manifest> {
    if root.join(".git").exists() {
        let revision = git(root, &["rev-parse", "HEAD"])?;
        let mut files = Vec::new();
        for record in git(root, &["ls-files", "--stage", "-z"])?
            .split('\0')
            .filter(|s| !s.is_empty())
        {
            let (attributes, path) = record
                .split_once('\t')
                .ok_or_else(|| invalid("invalid git index"))?;
            let fields: Vec<_> = attributes.split_whitespace().collect();
            if fields.len() != 3 || fields[2] != "0" {
                return Err(invalid("resolve index conflicts before building"));
            }
            files.push(Entry {
                path: path.to_owned(),
                mode: u32::from_str_radix(fields[0], 8)
                    .map_err(|_| invalid("invalid index mode"))?,
                sha256: String::new(),
            });
        }
        Ok(Manifest {
            format: 1,
            base_revision: revision.trim().to_owned(),
            tree_state:
                if git(root, &["status", "--porcelain", "--untracked-files=no"])?.is_empty() {
                    "clean"
                } else {
                    "modified"
                }
                .into(),
            files,
        })
    } else {
        serde_json::from_slice(&fs::read(root.join(MANIFEST))?).map_err(|e| invalid(e.to_string()))
    }
}

// A new compile input must be deliberately added to Git (or the exported
// inventory). Refuse to silently publish an incomplete corresponding source.
fn check_inputs(root: &Path, directory: &Path, paths: &BTreeSet<String>) -> io::Result<()> {
    for item in fs::read_dir(directory)? {
        let item = item?;
        let kind = item.file_type()?;
        if kind.is_dir() {
            if item.file_name() != "target" {
                check_inputs(root, &item.path(), paths)?;
            }
        } else {
            let relative = item
                .path()
                .strip_prefix(root)
                .unwrap()
                .to_str()
                .ok_or_else(|| invalid("non-UTF8 source path"))?
                .replace('\\', "/");
            if !paths.contains(&relative) {
                return Err(invalid(format!(
                    "unindexed compile input: {relative}; add source files to Git before building"
                )));
            }
        }
    }
    Ok(())
}

pub fn package(root: &Path) -> io::Result<Bundle> {
    let root = root.canonicalize()?;
    let mut manifest = inventory(&root)?;
    if manifest.format != 1 || manifest.files.is_empty() {
        return Err(invalid("unsupported or empty source manifest"));
    }
    manifest.files.sort_by(|a, b| a.path.cmp(&b.path));
    let paths: BTreeSet<_> = manifest
        .files
        .iter()
        .map(|entry| entry.path.clone())
        .collect();
    if paths.len() != manifest.files.len() {
        return Err(invalid("duplicate source entry"));
    }
    for required in [
        "Cargo.toml",
        "Cargo.lock",
        "rust-toolchain.toml",
        "LICENSE",
        "NOTICE",
        "README.md",
        "crates/ace-engine/build.rs",
    ] {
        if !paths.contains(required) {
            return Err(invalid(format!(
                "source inventory lacks {required}; add new source files to Git"
            )));
        }
    }
    for directory in ["crates", "tools/swarmtest", ".cargo"] {
        if root.join(directory).exists() {
            check_inputs(&root, &root.join(directory), &paths)?;
        }
    }
    let mut archive = tar::Builder::new(
        GzBuilder::new()
            .mtime(0)
            .write(Vec::new(), Compression::default()),
    );
    for entry in &mut manifest.files {
        let bytes = read_entry(&root, entry)?;
        #[cfg(unix)]
        if entry.mode != 0o120000 {
            use std::os::unix::fs::PermissionsExt;
            let executable =
                fs::metadata(root.join(&entry.path))?.permissions().mode() & 0o111 != 0;
            let mode = if executable { 0o100755 } else { 0o100644 };
            if entry.mode != mode {
                manifest.tree_state = "modified".into();
            }
            entry.mode = mode;
        }
        let digest = format!("{:x}", Sha256::digest(&bytes));
        if !entry.sha256.is_empty() && entry.sha256 != digest {
            manifest.tree_state = "modified".into();
        }
        entry.sha256 = digest;
        let mut header = tar::Header::new_gnu();
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(0);
        header.set_mode(entry.mode & 0o777);
        if entry.mode == 0o120000 {
            header.set_entry_type(tar::EntryType::Symlink);
            header.set_size(0);
            header.set_link_name(
                std::str::from_utf8(&bytes).map_err(|_| invalid("non-UTF8 symlink"))?,
            )?;
            header.set_cksum();
            archive.append_data(&mut header, format!("{PREFIX}/{}", entry.path), io::empty())?;
        } else {
            header.set_size(bytes.len() as u64);
            header.set_cksum();
            archive.append_data(&mut header, format!("{PREFIX}/{}", entry.path), &bytes[..])?;
        }
    }
    let bytes = serde_json::to_vec_pretty(&manifest).map_err(|e| invalid(e.to_string()))?;
    let mut header = tar::Header::new_gnu();
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
    header.set_mode(0o644);
    header.set_size(bytes.len() as u64);
    header.set_cksum();
    archive.append_data(&mut header, format!("{PREFIX}/{MANIFEST}"), &bytes[..])?;
    let bytes = archive.into_inner()?.finish()?;
    Ok(Bundle {
        sha256: format!("{:x}", Sha256::digest(&bytes)),
        bytes,
        paths: paths.into_iter().collect(),
        revision: manifest.base_revision,
        tree_state: manifest.tree_state,
    })
}
