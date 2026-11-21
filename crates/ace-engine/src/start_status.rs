//! Native stream start progress (`phase`) and stable failure codes for
//! `GET /streams/{network}/{id}/status` and the native start routes.
//!
//! The record of recent start attempts is bounded in size ([`START_ATTEMPT_CAPACITY`]) and in
//! time ([`START_ATTEMPT_TTL`] after a failure). It changes only at a few transitions per start
//! and is pruned lazily when touched: no background task, and nothing on the media path.

use crate::provider::ProviderError;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

/// Most start attempts remembered at once (in progress or recently failed).
pub(crate) const START_ATTEMPT_CAPACITY: usize = 64;
/// How long a failed attempt stays visible on the status route after it failed.
pub(crate) const START_ATTEMPT_TTL: Duration = Duration::from_secs(60);

type StreamKey = (String, String);

/// Where a live stream start currently is. Serialized as the lowercase `phase` string.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StartPhase {
    /// Resolving the id to its descriptor.
    Resolving,
    /// Descriptor known; looking for and connecting to peers.
    Connecting,
    /// Connected to at least one peer; waiting for the first media output.
    Buffering,
    /// The session has produced media output.
    Playing,
    /// The start failed; see the accompanying [`StartError`].
    Failed,
}

impl StartPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Resolving => "resolving",
            Self::Connecting => "connecting",
            Self::Buffering => "buffering",
            Self::Playing => "playing",
            Self::Failed => "failed",
        }
    }
}

/// Stable, machine-readable failure code. Clients key off this; the message is free text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StartErrorCode {
    /// Unknown network, or the start was refused without a more specific cause.
    NotFound,
    /// No peer answered metadata resolution, or none was found within the start budget.
    NoPeers,
    /// The network path (catalog hosts) was unreachable.
    Network,
    /// The id is well-formed but has no verified metadata to open it with.
    Unresolvable,
    /// Encrypted or invalid-protection content, which outpace does not play.
    Protected,
    /// The session (or conversion) capacity is exhausted; retry later.
    Capacity,
    /// Any other engine failure.
    Internal,
}

impl StartErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotFound => "not_found",
            Self::NoPeers => "no_peers",
            Self::Network => "network",
            Self::Unresolvable => "unresolvable",
            Self::Protected => "protected",
            Self::Capacity => "capacity",
            Self::Internal => "internal",
        }
    }

    /// Fixed user-facing text. Provider diagnostics may embed identifiers or URLs, so they
    /// stay in the operational log and never reach the response body.
    fn default_message(self) -> &'static str {
        match self {
            Self::NotFound => "stream not found",
            Self::NoPeers => "no peer is sharing this stream",
            Self::Network => "the network is unreachable",
            Self::Unresolvable => "stream metadata unavailable",
            Self::Protected => "protected content is not supported",
            Self::Capacity => "session capacity reached",
            Self::Internal => "stream start failed",
        }
    }
}

/// A typed start failure: `{"code": "...", "message": "..."}` on the wire.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StartError {
    pub code: StartErrorCode,
    pub message: String,
}

impl StartError {
    pub fn new(code: StartErrorCode) -> Self {
        Self {
            code,
            message: code.default_message().to_owned(),
        }
    }

    /// Classify a provider failure. Only messages documented as user-facing
    /// (`Unresolvable`, and the fixed protection texts) are passed through.
    pub fn from_provider(error: &ProviderError) -> Self {
        match error {
            ProviderError::NotFound => Self::new(StartErrorCode::NotFound),
            ProviderError::NoPeers(_) => Self::new(StartErrorCode::NoPeers),
            ProviderError::Network(_) | ProviderError::UnresolvableNetwork(_) => {
                Self::new(StartErrorCode::Network)
            }
            ProviderError::Unresolvable(reason) => {
                let mut error = Self::new(StartErrorCode::Unresolvable);
                // Only exact vetted static guidance is safe; providers may put
                // identifiers, signed URLs or credentials in other diagnostics.
                match reason.as_str() {
                    "this network does not support VOD"
                    | "source is single-file VOD; use /vod/ace/{id} or outpace play --vod"
                    | "catalog descriptor does not match the requested infohash"
                    | "an infohash alone is not a VOD target (no descriptor with piece hashes); use cid:<40hex> or a transport-url id"
                    | "source is live; use /streams/ace/{id}"
                    | "source is origin HLS; use /streams/ace/{id} or outpace play --resolve" => error.message = reason.clone(),
                    _ if reason.starts_with("no verified transport descriptor for infohash ") => {
                        error.message = "no verified transport descriptor; open by content id (cid:<content-id>) first".into();
                    },
                    _ => {},
                }
                error
            }
            ProviderError::ProtectedContent | ProviderError::InvalidProtection => Self {
                code: StartErrorCode::Protected,
                message: error.to_string(),
            },
            ProviderError::CapacityExhausted | ProviderError::ResolverBusy(_) => {
                Self::new(StartErrorCode::Capacity)
            }
            ProviderError::Backend(_)
            | ProviderError::Invalid(_)
            | ProviderError::DirectHls { .. } => Self::new(StartErrorCode::Internal),
        }
    }

    /// Classify an audio normalization failure that ended a session before any output:
    /// conversion admission is `capacity`, anything else `internal` with its fixed reason code.
    pub fn from_normalization(failure: crate::hls::HlsFailure) -> Self {
        if matches!(failure, crate::hls::HlsFailure::Admission) {
            Self::new(StartErrorCode::Capacity)
        } else {
            Self::new(StartErrorCode::Internal)
        }
    }

    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({ "code": self.code.as_str(), "message": self.message })
    }
}

/// A recorded start attempt that has no running session (yet, or any more).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum AttemptStatus {
    Resolving,
    Connecting,
    Failed(StartError),
}

impl AttemptStatus {
    pub fn phase(&self) -> StartPhase {
        match self {
            Self::Resolving => StartPhase::Resolving,
            Self::Connecting => StartPhase::Connecting,
            Self::Failed(_) => StartPhase::Failed,
        }
    }

    pub fn error(&self) -> Option<&StartError> {
        match self {
            Self::Failed(error) => Some(error),
            _ => None,
        }
    }
}

struct Attempt {
    key: StreamKey,
    seq: u64,
    status: AttemptStatus,
    failed_at: Option<Instant>,
}

#[derive(Default)]
struct AttemptLog {
    next_seq: u64,
    entries: VecDeque<Attempt>,
}

impl AttemptLog {
    fn prune(&mut self, now: Instant) {
        self.entries.retain(|attempt| {
            attempt
                .failed_at
                .is_none_or(|at| now.saturating_duration_since(at) < START_ATTEMPT_TTL)
        });
    }

    fn position(&self, key: &StreamKey, seq: u64) -> Option<usize> {
        self.entries
            .iter()
            .position(|attempt| attempt.seq == seq && attempt.key == *key)
    }
}

/// Bounded record of recent live start attempts, keyed like the manager's sessions.
#[derive(Default)]
pub(crate) struct StartAttempts {
    log: Mutex<AttemptLog>,
}

impl StartAttempts {
    /// Record a new attempt for `key` in [`AttemptStatus::Resolving`], replacing any earlier
    /// record for it. The returned guard removes the record when dropped unless
    /// [`StartAttempt::fail`] settled it, so success and cancellation leave nothing behind.
    pub fn begin(self: &Arc<Self>, key: StreamKey) -> StartAttempt {
        self.begin_at(key, Instant::now())
    }

    fn begin_at(self: &Arc<Self>, key: StreamKey, now: Instant) -> StartAttempt {
        let mut log = self.log.lock().unwrap();
        log.prune(now);
        if key.0.len().saturating_add(key.1.len()) > 8192 {
            // Retained observations have a byte bound as well as an entry bound.
            return StartAttempt {
                attempts: self.clone(),
                key,
                seq: 0,
                armed: false,
            };
        }
        log.entries.retain(|attempt| attempt.key != key);
        if log.entries.len() >= START_ATTEMPT_CAPACITY {
            // Evict the oldest failure first; in-progress attempts only when none is settled.
            let victim = log
                .entries
                .iter()
                .position(|attempt| attempt.failed_at.is_some())
                .unwrap_or(0);
            log.entries.remove(victim);
        }
        let seq = log.next_seq;
        let Some(next) = log.next_seq.checked_add(1) else {
            // Diagnostics must never recycle an identity that a delayed reporter
            // could still own. At exhaustion, omit the observation only.
            return StartAttempt {
                attempts: self.clone(),
                key,
                seq,
                armed: false,
            };
        };
        log.next_seq = next;
        log.entries.push_back(Attempt {
            key: key.clone(),
            seq,
            status: AttemptStatus::Resolving,
            failed_at: None,
        });
        StartAttempt {
            attempts: self.clone(),
            key,
            seq,
            armed: true,
        }
    }

    /// The recorded attempt for `key`, if it is in progress or failed within the TTL.
    pub fn status(&self, key: &StreamKey) -> Option<AttemptStatus> {
        self.status_at(key, Instant::now())
    }

    fn status_at(&self, key: &StreamKey, now: Instant) -> Option<AttemptStatus> {
        let mut log = self.log.lock().unwrap();
        log.prune(now);
        log.entries
            .iter()
            .find(|attempt| attempt.key == *key)
            .map(|attempt| attempt.status.clone())
    }

    fn update(&self, key: &StreamKey, seq: u64, status: AttemptStatus, now: Instant) {
        let mut log = self.log.lock().unwrap();
        if let Some(index) = log.position(key, seq) {
            let attempt = &mut log.entries[index];
            // Failure is terminal: a late progress report must not revive it or its expiry.
            if attempt.failed_at.is_some() {
                return;
            }
            attempt.failed_at = matches!(status, AttemptStatus::Failed(_)).then_some(now);
            attempt.status = status;
        }
    }

    pub(crate) fn clear(&self, key: &StreamKey) {
        self.log
            .lock()
            .unwrap()
            .entries
            .retain(|attempt| &attempt.key != key);
    }

    fn remove(&self, key: &StreamKey, seq: u64) {
        let mut log = self.log.lock().unwrap();
        if let Some(index) = log.position(key, seq) {
            log.entries.remove(index);
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.log.lock().unwrap().entries.len()
    }
}

/// Owner of one in-progress attempt record.
pub(crate) struct StartAttempt {
    attempts: Arc<StartAttempts>,
    key: StreamKey,
    seq: u64,
    armed: bool,
}

impl StartAttempt {
    /// A handle the provider uses to report progress for this attempt.
    pub fn progress(&self) -> StartProgress {
        if !self.armed {
            return StartProgress::none();
        }
        StartProgress {
            target: Some((Arc::downgrade(&self.attempts), self.key.clone(), self.seq)),
        }
    }

    /// Settle the attempt as failed; it stays visible for [`START_ATTEMPT_TTL`].
    pub fn fail(mut self, error: &ProviderError) {
        self.armed = false;
        self.attempts.update(
            &self.key,
            self.seq,
            AttemptStatus::Failed(StartError::from_provider(error)),
            Instant::now(),
        );
    }
}

impl Drop for StartAttempt {
    fn drop(&mut self) {
        if self.armed {
            self.attempts.remove(&self.key, self.seq);
        }
    }
}

/// Progress reporter passed to [`crate::provider::StreamProvider::open_with_progress`].
/// A default (detached) reporter ignores every report.
#[derive(Clone, Default)]
pub struct StartProgress {
    target: Option<(Weak<StartAttempts>, StreamKey, u64)>,
}

impl StartProgress {
    pub fn none() -> Self {
        Self::default()
    }

    /// The descriptor is known; the provider is now looking for and connecting to peers.
    pub fn resolved(&self) {
        if let Some((attempts, key, seq)) = &self.target {
            if let Some(attempts) = attempts.upgrade() {
                attempts.update(key, *seq, AttemptStatus::Connecting, Instant::now());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn key(id: &str) -> StreamKey {
        ("synthetic".into(), id.into())
    }

    #[test]
    fn bounded_attempts_expire_failures_and_never_recycle_reporters() {
        let attempts = Arc::new(StartAttempts::default());
        let now = Instant::now();
        let first = attempts.begin_at(key("same"), now);
        let stale = first.progress();
        let replacement = attempts.begin_at(key("same"), now);
        first.fail(&ProviderError::Network("private diagnostic".into()));
        stale.resolved();
        assert_eq!(
            attempts.status(&key("same")),
            Some(AttemptStatus::Resolving)
        );
        let progress = replacement.progress();
        progress.resolved();
        assert_eq!(
            attempts.status(&key("same")),
            Some(AttemptStatus::Connecting)
        );
        replacement.fail(&ProviderError::NoPeers("private diagnostic".into()));
        progress.resolved();
        assert!(matches!(
            attempts.status(&key("same")),
            Some(AttemptStatus::Failed(StartError {
                code: StartErrorCode::NoPeers,
                ..
            }))
        ));
        assert!(attempts
            .status_at(
                &key("same"),
                now + START_ATTEMPT_TTL + Duration::from_secs(1)
            )
            .is_none());
        let mut guards = Vec::new();
        for index in 0..START_ATTEMPT_CAPACITY + 10 {
            guards.push(attempts.begin(key(&index.to_string())));
        }
        assert_eq!(attempts.len(), START_ATTEMPT_CAPACITY);
        let before = attempts.len();
        let _oversized = attempts.begin(key(&"x".repeat(8193)));
        assert_eq!(attempts.len(), before);
        attempts.log.lock().unwrap().next_seq = u64::MAX;
        let exhausted = attempts.begin(key("exhausted"));
        exhausted.progress().resolved();
        assert!(attempts.status(&key("exhausted")).is_none());
        drop(guards);
    }

    #[test]
    fn cancellation_and_stop_clear_observation_without_late_revival() {
        let attempts = Arc::new(StartAttempts::default());
        let opening = attempts.begin(key("cancelled"));
        let progress = opening.progress();
        drop(opening);
        progress.resolved();
        assert!(attempts.status(&key("cancelled")).is_none());
        let stopped = attempts.begin(key("stopped"));
        attempts.clear(&key("stopped"));
        stopped.fail(&ProviderError::Network("private diagnostic".into()));
        assert!(attempts.status(&key("stopped")).is_none());
    }

    #[test]
    fn failure_codes_are_typed_and_do_not_publish_provider_diagnostics() {
        let secret = "https://private.invalid/path?token=do-not-publish";
        for (error, expected) in [
            (ProviderError::NotFound, "not_found"),
            (ProviderError::NoPeers(secret.into()), "no_peers"),
            (ProviderError::Network(secret.into()), "network"),
            (ProviderError::ResolverBusy(secret.into()), "capacity"),
            (ProviderError::UnresolvableNetwork(secret.into()), "network"),
            (ProviderError::Unresolvable(secret.into()), "unresolvable"),
            (ProviderError::ProtectedContent, "protected"),
            (ProviderError::Backend(secret.into()), "internal"),
        ] {
            let failure = StartError::from_provider(&error);
            assert_eq!(failure.code.as_str(), expected);
            assert!(!failure.to_json().to_string().contains(secret));
        }
        let static_guidance = "source is live; use /streams/ace/{id}";
        assert_eq!(
            StartError::from_provider(&ProviderError::Unresolvable(static_guidance.into())).message,
            static_guidance
        );
    }
}
