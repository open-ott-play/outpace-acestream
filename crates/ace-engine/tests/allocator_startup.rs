use std::process::Stdio;
use std::time::Duration;

/// Exercise allocator initialization in the actual daemon binary: a library-only test does not
/// use the binary's global allocator or its exported configuration.
#[tokio::test]
async fn version_starts_without_allocator_diagnostics() {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_outpace"));
    command
        .arg("--version")
        // jemalloc accepts either name depending on whether its symbols are prefixed. An
        // inherited override must not hide broken compiled-in defaults from this smoke test.
        .env_remove("MALLOC_CONF")
        .env_remove("_RJEM_MALLOC_CONF")
        .stdin(Stdio::null())
        .kill_on_drop(true);

    let output = tokio::time::timeout(Duration::from_secs(15), command.output())
        .await
        .expect("outpace --version must finish within 15 seconds")
        .expect("outpace --version must launch");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "outpace --version failed: status={}, stdout={stdout:?}, stderr={stderr:?}",
        output.status,
    );
    assert_eq!(
        stdout.trim(),
        format!("outpace {}", env!("CARGO_PKG_VERSION"))
    );
    assert!(
        output.stderr.is_empty(),
        "outpace --version emitted startup diagnostics: {stderr:?}",
    );
}
