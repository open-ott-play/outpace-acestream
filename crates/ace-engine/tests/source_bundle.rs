#[allow(dead_code)]
#[path = "../build_support/source_bundle.rs"]
mod source_bundle;

use flate2::read::GzDecoder;
use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT: AtomicUsize = AtomicUsize::new(0);
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "outpace-source-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        for name in [
            "Cargo.toml",
            "Cargo.lock",
            "rust-toolchain.toml",
            "LICENSE",
            "NOTICE",
            "README.md",
            "crates/ace-engine/build.rs",
        ] {
            fs::create_dir_all(root.join(name).parent().unwrap()).unwrap();
            fs::write(root.join(name), "initial source\n").unwrap();
        }
        git(&root, &["init", "--quiet"]);
        git(&root, &["add", "."]);
        git(
            &root,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.invalid",
                "-c",
                "core.hooksPath=missing-hooks",
                "commit",
                "--quiet",
                "-m",
                "initial",
            ],
        );
        Self(root)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn git(root: &Path, args: &[&str]) {
    assert!(Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .status()
        .unwrap()
        .success());
}
fn contents(bytes: &[u8]) -> BTreeMap<String, Vec<u8>> {
    let mut archive = tar::Archive::new(GzDecoder::new(bytes));
    archive
        .entries()
        .unwrap()
        .map(|entry| {
            let mut entry = entry.unwrap();
            let path = entry.path().unwrap().to_str().unwrap().to_owned();
            let mut data = Vec::new();
            entry.read_to_end(&mut data).unwrap();
            (path, data)
        })
        .collect()
}

#[test]
fn archive_contains_actual_dirty_sources_and_omits_private_untracked_files() {
    let fixture = Fixture::new();
    fs::write(fixture.0.join("README.md"), "changed source\n").unwrap();
    fs::write(fixture.0.join("private-operator.env"), "PRIVATE_SENTINEL").unwrap();
    let bundle = source_bundle::package(&fixture.0).unwrap();
    assert_eq!(bundle.tree_state, "modified");
    let files = contents(&bundle.bytes);
    assert_eq!(files["outpace-acestream/README.md"], b"changed source\n");
    assert!(!files
        .keys()
        .any(|path| path.contains("private-operator") || path.contains(".git/")));
    assert_eq!(
        bundle.bytes,
        source_bundle::package(&fixture.0).unwrap().bytes
    );
}

#[test]
fn exported_archive_rebuilds_without_git_and_marks_later_changes() {
    let fixture = Fixture::new();
    let original = source_bundle::package(&fixture.0).unwrap();
    let export = fixture.0.join("export");
    fs::create_dir(&export).unwrap();
    tar::Archive::new(GzDecoder::new(&original.bytes[..]))
        .unpack(&export)
        .unwrap();
    let root = export.join("outpace-acestream");
    assert_eq!(source_bundle::package(&root).unwrap().bytes, original.bytes);
    fs::write(root.join("README.md"), "downstream modification").unwrap();
    let changed = source_bundle::package(&root).unwrap();
    assert_eq!(changed.tree_state, "modified");
    assert_ne!(changed.sha256, original.sha256);
}

#[test]
fn new_compile_inputs_must_be_included_deliberately() {
    let fixture = Fixture::new();
    fs::write(
        fixture.0.join("crates/ace-engine/new.rs"),
        "fn missing() {}",
    )
    .unwrap();
    assert!(source_bundle::package(&fixture.0)
        .err()
        .unwrap()
        .to_string()
        .contains("unindexed compile input"));
    git(&fixture.0, &["add", "crates/ace-engine/new.rs"]);
    assert!(contents(&source_bundle::package(&fixture.0).unwrap().bytes)
        .contains_key("outpace-acestream/crates/ace-engine/new.rs"));
}

#[cfg(unix)]
#[test]
fn replaced_tracked_file_cannot_leak_external_data_through_a_symlink() {
    let fixture = Fixture::new();
    fs::write(fixture.0.join("private-operator.env"), "PRIVATE_SENTINEL").unwrap();
    fs::remove_file(fixture.0.join("README.md")).unwrap();
    std::os::unix::fs::symlink("private-operator.env", fixture.0.join("README.md")).unwrap();
    assert!(source_bundle::package(&fixture.0).is_err());
}

#[test]
fn exported_inventory_cannot_escape_the_source_root() {
    let fixture = Fixture::new();
    let bundle = source_bundle::package(&fixture.0).unwrap();
    let export = fixture.0.join("export");
    fs::create_dir(&export).unwrap();
    tar::Archive::new(GzDecoder::new(&bundle.bytes[..]))
        .unpack(&export)
        .unwrap();
    let root = export.join("outpace-acestream");
    let path = root.join("SOURCE-MANIFEST.json");
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    manifest["files"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!({"path":"../../private-operator.env", "mode":33188, "sha256":""}));
    fs::write(path, serde_json::to_vec(&manifest).unwrap()).unwrap();
    assert!(source_bundle::package(&root).is_err());
}

#[test]
fn sha256_representation_preserves_manifest_and_archive_hashes() {
    use sha2::{Digest, Sha256};
    let fixture = Fixture::new();
    fs::write(fixture.0.join("README.md"), b"abc").unwrap();
    let bundle = source_bundle::package(&fixture.0).unwrap();
    let files = contents(&bundle.bytes);
    let manifest: serde_json::Value =
        serde_json::from_slice(&files["outpace-acestream/SOURCE-MANIFEST.json"]).unwrap();
    let readme = manifest["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["path"] == "README.md")
        .unwrap();
    assert_eq!(
        readme["sha256"],
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    assert_eq!(bundle.sha256, hex::encode(Sha256::digest(&bundle.bytes)));
    assert_eq!(bundle.sha256.len(), 64);
}
