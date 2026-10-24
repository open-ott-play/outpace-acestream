//! Operator-only, bounded peer-history probe. Never reads a content-ID registry.
//!
//! Supply ACE_CONTENT_ID and an absolute, NEW ACE_HISTORY_OUTPUT directory through
//! the environment. Optional ACE_HISTORY_BEHIND_PIECES (default 32, max 1024) and
//! ACE_HISTORY_PIECES (default 8, max 32) select non-overlapping old/reference ranges.
//! Example: `cargo run -p ace-swarm --example history_probe`.
//!
//! The entire probe has a 90-second deadline, at most four candidate peers per
//! operation, and at most 32 MiB per range. It writes private raw media and a JSON
//! report. Successful acquisition is NOT proof of historical decoded media: compare
//! PCR/decoded frame timestamps in both files, check discontinuities and repeated
//! payloads, and never use unauthenticated peer-header times as the verdict.

use ace_media::mpegts::{ts_timing, TsResync};
use ace_swarm::discover::{discover_peers_with_options, DiscoveryOptions};
use ace_swarm::history::{
    fetch_history, inspect_history_windows, HistoryLimits, HistoryRange, HistoryRequest,
};
use ace_swarm::resolve::resolve_via_catalog;
use ace_wire::handshake::random_peer_id;
use ace_wire::identity::Identity;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs::{DirBuilder, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::time::timeout;

type ProbeResult<T> = Result<T, Box<dyn std::error::Error>>;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let result = timeout(Duration::from_secs(90), run()).await;
    match result {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            eprintln!("history probe: {error}");
            std::process::exit(1);
        }
        Err(_) => {
            eprintln!(
                "history probe: 90-second total deadline reached; no claim of archive availability"
            );
            std::process::exit(1);
        }
    }
}

fn integer_env(name: &str, default: u32, max: u32) -> ProbeResult<u32> {
    let value = match std::env::var(name) {
        Ok(value) => value
            .parse::<u32>()
            .map_err(|_| format!("{name} must be an integer"))?,
        Err(_) => default,
    };
    if value == 0 || value > max {
        return Err(format!("{name} outside 1..={max}").into());
    }
    Ok(value)
}

async fn run() -> ProbeResult<()> {
    let content_id = std::env::var("ACE_CONTENT_ID").map_err(|_| "ACE_CONTENT_ID is required")?;
    let output = PathBuf::from(
        std::env::var("ACE_HISTORY_OUTPUT").map_err(|_| "ACE_HISTORY_OUTPUT is required")?,
    );
    if !output.is_absolute() {
        return Err("ACE_HISTORY_OUTPUT must be an absolute new directory".into());
    }
    let count = integer_env("ACE_HISTORY_PIECES", 8, 32)?;
    let behind = integer_env("ACE_HISTORY_BEHIND_PIECES", 32, 1024)?;
    if behind < count {
        return Err("history and reference ranges must not overlap".into());
    }
    let mut directory = DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        directory.mode(0o700);
    }
    directory.create(&output)?;
    // No cache: bind both comparisons to one freshly authenticated catalog descriptor.
    let info = timeout(Duration::from_secs(12), resolve_via_catalog(&content_id))
        .await
        .map_err(|_| "catalog deadline reached")?
        .map_err(|_| "catalog resolution failed")?;
    let peers = timeout(
        Duration::from_secs(10),
        discover_peers_with_options(
            &info.trackers,
            &info.infohash,
            &random_peer_id(),
            0,
            DiscoveryOptions {
                peer_target: 4,
                dht_budget: Duration::from_secs(8),
            },
        ),
    )
    .await
    .map_err(|_| "peer discovery deadline reached")?;
    let limits = HistoryLimits {
        max_peers: 4,
        max_bytes: 32 * 1024 * 1024,
        ..Default::default()
    };
    let windows = inspect_history_windows(
        &info,
        &peers,
        HistoryLimits {
            total_timeout: Duration::from_secs(12),
            ..limits
        },
    )
    .await?;
    let head = windows
        .iter()
        .map(|window| window.max_piece)
        .max()
        .ok_or("no usable peer window")?;
    let reference_start = head
        .checked_sub(count - 1)
        .ok_or("reference range underflows")?;
    let history_start = reference_start
        .checked_sub(behind)
        .ok_or("historical range underflows")?;
    if !windows.iter().any(|window| {
        window.min_piece <= history_start && window.max_piece >= history_start + count - 1
    }) {
        return Err("requested historical range is not advertised by any inspected peer".into());
    }
    let identity = Identity::generate();
    let historical = fetch_history(
        &info,
        &identity,
        &peers,
        HistoryRequest {
            start_piece: history_start,
            piece_count: count as u16,
        },
        limits,
    )
    .await?;
    let history_bytes = payload(&historical);
    write_private(&output.join("history.ts"), &history_bytes)?;
    // If this fails the partial raw artifact is diagnostic only; no success report is written.
    let reference = fetch_history(
        &info,
        &identity,
        &peers,
        HistoryRequest {
            start_piece: reference_start,
            piece_count: count as u16,
        },
        limits,
    )
    .await?;
    let reference_bytes = payload(&reference);
    write_private(&output.join("reference.ts"), &reference_bytes)?;
    let report = json!({
        "schema": 1,
        "result": "verified_piece_acquisition_only",
        "decoded_history_proven": false,
        "note": "Source signatures bind payload only, not wire indices or header timestamps. Decode both files and compare media clocks before claiming backward playback.",
        "history": describe(&historical, &history_bytes),
        "reference": describe(&reference, &reference_bytes),
        "identical_payload": history_bytes == reference_bytes,
        "advertised_head": head,
        "behind_pieces": behind,
        "inspected_windows": windows.iter().map(|window| json!({"min_piece": window.min_piece, "max_piece": window.max_piece})).collect::<Vec<_>>()
    });
    write_private(
        &output.join("report.json"),
        &serde_json::to_vec_pretty(&report)?,
    )?;
    eprintln!("history probe: acquired two verified ranges; inspect private report/media and decode timestamps before calling this a successful rewind");
    Ok(())
}

fn payload(range: &HistoryRange) -> Vec<u8> {
    let mut bytes = Vec::new();
    for piece in &range.pieces {
        bytes.extend_from_slice(&piece.payload);
    }
    bytes
}

#[derive(Default)]
struct PcrSummary {
    first: Option<u64>,
    last: Option<u64>,
    samples: usize,
    discontinuities: usize,
}

fn describe(range: &HistoryRange, bytes: &[u8]) -> Value {
    let mut pcr: BTreeMap<u16, PcrSummary> = BTreeMap::new();
    let aligned = TsResync::new().push(bytes);
    for packet in aligned.as_chunks::<188>().0 {
        let timing = ts_timing(packet);
        if timing.pcr.is_none() && !timing.discontinuity {
            continue;
        }
        let clock = pcr.entry(timing.pid).or_default();
        clock.discontinuities += usize::from(timing.discontinuity);
        if let Some(value) = timing.pcr {
            clock.first.get_or_insert(value);
            clock.last = Some(value);
            clock.samples += 1;
        }
    }
    json!({
        "pieces": range.pieces.iter().map(|piece| json!({"index": piece.index, "advisory_unix_seconds": piece.reported_unix_seconds})).collect::<Vec<_>>(),
        "bytes": bytes.len(),
        "attempted_peers": range.attempted_peers,
        "pcr_90khz": pcr.iter().map(|(pid, clock)| json!({"pid": pid, "first": clock.first, "last": clock.last, "samples": clock.samples, "discontinuities": clock.discontinuities})).collect::<Vec<_>>()
    })
}

fn write_private(path: &Path, bytes: &[u8]) -> ProbeResult<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)?.write_all(bytes)?;
    Ok(())
}
