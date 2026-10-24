#[path = "build_support/source_bundle.rs"]
mod source_bundle;

fn main() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let output = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    let bundle = source_bundle::package(&root).expect("cannot prepare corresponding source");
    // Watch the whole source tree so newly added source files cannot silently
    // compile without being reflected in the embedded source inventory.
    for directory in ["crates", "tools", "docs", "tests", ".github"] {
        println!("cargo:rerun-if-changed={}", root.join(directory).display());
    }
    for path in &bundle.paths {
        println!("cargo:rerun-if-changed={}", root.join(path).display());
    }
    if root.join("SOURCE-MANIFEST.json").exists() {
        println!(
            "cargo:rerun-if-changed={}",
            root.join("SOURCE-MANIFEST.json").display()
        );
    }
    // The worktree's .git can be a file; resolve the actual index and HEAD.
    for name in ["HEAD", "index", "refs"] {
        if let Ok(path) = source_bundle::git_path(&root, name) {
            if std::path::Path::new(&path).exists() {
                println!("cargo:rerun-if-changed={path}");
            }
        }
    }
    let compiler = std::process::Command::new(std::env::var_os("RUSTC").unwrap())
        .arg("--version")
        .output()
        .expect("cannot identify Rust compiler");
    assert!(compiler.status.success(), "cannot identify Rust compiler");
    println!("cargo:rerun-if-env-changed=OUTPACE_RELEASE_BUILD");
    let release_build = std::env::var("OUTPACE_RELEASE_BUILD").as_deref() == Ok("1");
    let version = std::env::var("CARGO_PKG_VERSION").unwrap();
    let target = std::env::var("TARGET").unwrap();
    let complete_source_url = if release_build {
        assert_eq!(
            bundle.tree_state, "clean",
            "release source must come from a clean tracked tree"
        );
        Some(format!("https://github.com/open-ott-play/outpace-acestream/releases/download/v{version}/outpace-source-{version}-{target}-{}.tar.gz", bundle.revision))
    } else {
        None
    };
    let metadata = serde_json::json!({
        "project": "Outpace AceStream",
        "repository": "https://github.com/open-ott-play/outpace-acestream",
        "version": std::env::var("CARGO_PKG_VERSION").unwrap(),
        "base_revision": bundle.revision,
        "tree_state": bundle.tree_state,
        "source_sha256": bundle.sha256,
        "target": std::env::var("TARGET").unwrap(),
        "profile": std::env::var("PROFILE").unwrap(),
        "rustc": String::from_utf8(compiler.stdout).unwrap().trim(),
        "source_url": "/source.tar.gz",
        "complete_source_url": complete_source_url,
        "license": "AGPL-3.0-or-later",
    });
    std::fs::write(output.join("corresponding-source.tar.gz"), bundle.bytes).unwrap();
    std::fs::write(
        output.join("source.json"),
        serde_json::to_vec_pretty(&metadata).unwrap(),
    )
    .unwrap();
}
