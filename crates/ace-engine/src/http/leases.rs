use crate::session::Subscription;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const ACE_TOKEN_TTL: Duration = Duration::from_secs(6 * 60 * 60);
const ACE_TOKEN_CAPACITY: usize = 4096;

pub(super) struct AceLease {
    playback_id: String,
    session_key: String,
    expires_at: Instant,
    cancel: tokio::sync::watch::Sender<bool>,
    /// An HLS client has no long-lived response body to own its subscriber count. Keep one
    /// receiver in the bounded lease instead, so the manager reaper sees the client until stop,
    /// expiry, or capacity eviction. The receiver is never read; the packager uses its own raw
    /// receiver and remains the sole segmenting pipeline.
    hls: Option<AceHlsLease>,
}

struct AceHlsLease {
    _pin: Subscription,
    idle_deadline: Instant,
    pending_requests: usize,
}

impl AceLease {
    fn deadline(&self) -> Instant {
        match &self.hls {
            Some(hls) if hls.pending_requests == 0 => self.expires_at.min(hls.idle_deadline),
            _ => self.expires_at,
        }
    }
}

pub(super) struct AceHlsRequestGuard {
    sessions: Arc<AceSessionStore>,
    token: String,
}

impl Drop for AceHlsRequestGuard {
    fn drop(&mut self) {
        let mut leases = self.sessions.leases.lock().unwrap();
        if let Some(hls) = leases
            .get_mut(&self.token)
            .and_then(|lease| lease.hls.as_mut())
        {
            hls.pending_requests -= 1;
            hls.idle_deadline = hls
                .idle_deadline
                .max(Instant::now() + self.sessions.hls_idle_timeout);
        }
        drop(leases);
        self.sessions.wake_hls_reaper();
    }
}

/// Owns a hidden direct-playback lease for exactly as long as its HTTP body exists.
pub(super) struct AceDirectLeaseGuard {
    pub(super) sessions: Arc<AceSessionStore>,
    pub(super) playback_id: String,
    pub(super) token: String,
}

impl Drop for AceDirectLeaseGuard {
    fn drop(&mut self) {
        self.sessions.remove_owned(&self.playback_id, &self.token);
    }
}

/// Bounded playback leases minted by `/ace/getstream`. A token is both authorization and the
/// compatibility client's ownership handle; revoking one never force-stops the shared source.
pub struct AceSessionStore {
    pub(super) leases: Mutex<HashMap<String, AceLease>>,
    ttl: Duration,
    hls_idle_timeout: Duration,
    capacity: usize,
    /// A single weak-reference reaper handles every HLS deadline for this store. Mutations bump
    /// the watch generation so capacity eviction, stop, and revoke cancel its current sleep and
    /// make it recalculate the next live deadline.
    hls_reaper_signal: tokio::sync::watch::Sender<u64>,
    hls_reaper_started: AtomicBool,
    hls_reaper_workers: Arc<AtomicUsize>,
}

struct HlsReaperGuard(Arc<AtomicUsize>);

impl Drop for HlsReaperGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Default for AceSessionStore {
    fn default() -> Self {
        Self::with_hls_idle_timeout(Duration::from_secs(60))
    }
}

impl AceSessionStore {
    pub(crate) fn with_hls_idle_timeout(idle: Duration) -> Self {
        Self::with_policy(ACE_TOKEN_TTL, ACE_TOKEN_CAPACITY, idle)
    }

    #[cfg(test)]
    pub(super) fn new(ttl: Duration, capacity: usize) -> Self {
        Self::with_policy(ttl, capacity, Duration::from_secs(60))
    }

    pub(super) fn with_policy(ttl: Duration, capacity: usize, hls_idle_timeout: Duration) -> Self {
        let (hls_reaper_signal, _) = tokio::sync::watch::channel(0);
        Self {
            leases: Mutex::new(HashMap::new()),
            ttl,
            hls_idle_timeout,
            capacity,
            hls_reaper_signal,
            hls_reaper_started: AtomicBool::new(false),
            hls_reaper_workers: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub(super) fn mint(&self, playback_id: String, session_key: String) -> String {
        use rand::TryRng;
        let mut bytes = [0_u8; 32];
        rand::rngs::SysRng
            .try_fill_bytes(&mut bytes)
            .expect("OS randomness for playback token");
        let token = hex::encode(bytes);
        self.insert_at(token.clone(), playback_id, session_key, Instant::now());
        token
    }

    pub(super) fn mint_hls(
        self: &Arc<Self>,
        playback_id: String,
        session_key: String,
        pin: Subscription,
    ) -> String {
        use rand::TryRng;
        let mut bytes = [0_u8; 32];
        rand::rngs::SysRng
            .try_fill_bytes(&mut bytes)
            .expect("OS randomness for playback token");
        let token = hex::encode(bytes);
        self.insert_hls_at(token.clone(), playback_id, session_key, Instant::now(), pin);
        self.ensure_hls_reaper();
        self.wake_hls_reaper();
        token
    }

    fn ensure_hls_reaper(self: &Arc<Self>) {
        if self
            .hls_reaper_started
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return;
        }
        let weak_store = Arc::downgrade(self);
        let mut signal = self.hls_reaper_signal.subscribe();
        let workers = self.hls_reaper_workers.clone();
        workers.fetch_add(1, Ordering::SeqCst);
        tokio::spawn(async move {
            let _guard = HlsReaperGuard(workers);
            loop {
                let Some(store) = weak_store.upgrade() else {
                    break;
                };
                let deadline = store.next_hls_expiry();
                drop(store);

                match deadline {
                    Some(deadline) => {
                        tokio::select! {
                            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {
                                if let Some(store) = weak_store.upgrade() {
                                    store.purge_expired(Instant::now());
                                } else {
                                    break;
                                }
                            }
                            changed = signal.changed() => {
                                if changed.is_err() {
                                    break;
                                }
                            }
                        }
                    }
                    None => {
                        if signal.changed().await.is_err() {
                            break;
                        }
                    }
                }
            }
        });
    }

    fn next_hls_expiry(&self) -> Option<Instant> {
        self.leases
            .lock()
            .unwrap()
            .values()
            .filter(|lease| lease.hls.is_some())
            .map(AceLease::deadline)
            .min()
    }

    fn wake_hls_reaper(&self) {
        self.hls_reaper_signal
            .send_modify(|generation| *generation = generation.wrapping_add(1));
    }

    fn purge_expired(&self, now: Instant) {
        let mut leases = self.leases.lock().unwrap();
        leases.retain(|_, lease| {
            let keep = lease.deadline() > now;
            if !keep {
                let _ = lease.cancel.send(true);
            }
            keep
        });
    }

    pub(super) fn insert_at(
        &self,
        token: String,
        playback_id: String,
        session_key: String,
        now: Instant,
    ) {
        self.insert_with_pin(token, playback_id, session_key, now, None);
    }

    fn insert_with_pin(
        &self,
        token: String,
        playback_id: String,
        session_key: String,
        now: Instant,
        hls_pin: Option<Subscription>,
    ) {
        let mut hls_schedule_changed = hls_pin.is_some();
        self.purge_expired(now);
        let mut leases = self.leases.lock().unwrap();
        if leases.len() >= self.capacity {
            if let Some(oldest) = leases
                .iter()
                .min_by_key(|(_, lease)| lease.deadline())
                .map(|(t, _)| t.clone())
            {
                if let Some(lease) = leases.remove(&oldest) {
                    hls_schedule_changed |= lease.hls.is_some();
                    let _ = lease.cancel.send(true);
                }
            }
        }
        let (cancel, _) = tokio::sync::watch::channel(false);
        leases.insert(
            token,
            AceLease {
                playback_id,
                session_key,
                expires_at: now + self.ttl,
                cancel,
                hls: hls_pin.map(|pin| AceHlsLease {
                    _pin: pin,
                    idle_deadline: now + self.hls_idle_timeout,
                    pending_requests: 0,
                }),
            },
        );
        drop(leases);
        if hls_schedule_changed {
            self.wake_hls_reaper();
        }
    }

    pub(super) fn insert_hls_at(
        &self,
        token: String,
        playback_id: String,
        session_key: String,
        now: Instant,
        pin: Subscription,
    ) {
        self.insert_with_pin(token, playback_id, session_key, now, Some(pin));
    }

    /// Protect an authorized HLS manifest wait from idle expiry. Absolute authorization
    /// expiry, stop and eviction still win. Dropping the request releases the protection.
    pub(super) fn begin_hls_request(
        self: &Arc<Self>,
        playback_id: &str,
        token: &str,
    ) -> Option<AceHlsRequestGuard> {
        let now = Instant::now();
        self.purge_expired(now);
        let mut leases = self.leases.lock().unwrap();
        let lease = leases
            .get_mut(token)
            .filter(|lease| lease.playback_id == playback_id)?;
        let hls = lease.hls.as_mut()?;
        hls.pending_requests += 1;
        hls.idle_deadline = hls.idle_deadline.max(now + self.hls_idle_timeout);
        drop(leases);
        self.wake_hls_reaper();
        Some(AceHlsRequestGuard {
            sessions: self.clone(),
            token: token.to_string(),
        })
    }

    pub(super) fn validate(&self, playback_id: &str, token: &str) -> Option<String> {
        self.validate_at(playback_id, token, Instant::now())
    }

    pub(super) fn validate_token(&self, token: &str) -> Option<(String, String)> {
        let now = Instant::now();
        self.purge_expired(now);
        self.leases
            .lock()
            .unwrap()
            .get(token)
            .map(|lease| (lease.playback_id.clone(), lease.session_key.clone()))
    }

    pub(super) fn validate_at(
        &self,
        playback_id: &str,
        token: &str,
        now: Instant,
    ) -> Option<String> {
        self.purge_expired(now);
        self.leases
            .lock()
            .unwrap()
            .get(token)
            .filter(|lease| lease.playback_id == playback_id)
            .map(|lease| lease.session_key.clone())
    }

    pub(super) fn revoke(&self, playback_id: &str, token: &str) -> bool {
        let mut leases = self.leases.lock().unwrap();
        let matches = leases.get(token).is_some_and(|lease| {
            lease.playback_id == playback_id && lease.deadline() > Instant::now()
        });
        if matches {
            if let Some(lease) = leases.remove(token) {
                let _ = lease.cancel.send(true);
            }
        }
        drop(leases);
        if matches {
            self.wake_hls_reaper();
        }
        matches
    }

    /// Remove a lease owned by an internal body guard even if its authorization deadline has
    /// just passed. Unlike `revoke`, this is not an authorization check: the guard was created
    /// with the exact id/token pair when the lease was minted and must always clean up its entry.
    fn remove_owned(&self, playback_id: &str, token: &str) -> bool {
        let mut leases = self.leases.lock().unwrap();
        let matches = leases
            .get(token)
            .is_some_and(|lease| lease.playback_id == playback_id);
        if matches {
            if let Some(lease) = leases.remove(token) {
                let _ = lease.cancel.send(true);
            }
        }
        drop(leases);
        if matches {
            self.wake_hls_reaper();
        }
        matches
    }

    pub(super) fn playback(
        &self,
        playback_id: &str,
        token: &str,
    ) -> Option<(String, Instant, tokio::sync::watch::Receiver<bool>)> {
        let now = Instant::now();
        self.purge_expired(now);
        self.leases
            .lock()
            .unwrap()
            .get(token)
            .filter(|lease| lease.playback_id == playback_id)
            .map(|lease| {
                (
                    lease.session_key.clone(),
                    lease.expires_at,
                    lease.cancel.subscribe(),
                )
            })
    }

    #[cfg(test)]
    pub(super) fn active_count(&self) -> usize {
        self.leases.lock().unwrap().len()
    }

    #[cfg(test)]
    pub(super) fn hls_reaper_workers(&self) -> Arc<AtomicUsize> {
        self.hls_reaper_workers.clone()
    }
}

#[cfg(test)]
mod idle_tests {
    use super::*;
    use crate::provider::TsSource;
    use crate::session::StreamSession;

    struct PendingSource;
    #[async_trait::async_trait]
    impl TsSource for PendingSource {
        fn stats(&self) -> crate::provider::SourceStats {
            crate::provider::SourceStats::default()
        }
        async fn next(&mut self) -> Option<bytes::Bytes> {
            std::future::pending().await
        }
    }

    #[tokio::test]
    async fn idle_is_per_client_and_control_polling_does_not_renew_it() {
        let now = Instant::now();
        let store = Arc::new(AceSessionStore::with_policy(
            Duration::from_secs(300),
            8,
            Duration::from_secs(60),
        ));
        let source = StreamSession::start(Box::new(PendingSource), 8);
        for token in ["abandoned", "watching"] {
            store.insert_hls_at(
                token.into(),
                "shared".into(),
                "key".into(),
                now - Duration::from_secs(59),
                source.subscribe(),
            );
        }
        assert!(store.validate_at("shared", "abandoned", now).is_some());
        drop(store.begin_hls_request("shared", "watching").unwrap());
        store.purge_expired(now + Duration::from_secs(2));
        assert_eq!(source.subscriber_count(), 1);
        assert!(!store.leases.lock().unwrap().contains_key("abandoned"));
        assert!(store.leases.lock().unwrap().contains_key("watching"));
        store.purge_expired(now + Duration::from_secs(61));
        assert_eq!(source.subscriber_count(), 0);
        assert!(store.begin_hls_request("shared", "abandoned").is_none());
    }

    #[tokio::test]
    async fn pending_request_protects_startup_but_not_absolute_expiry_or_stop() {
        let now = Instant::now();
        let store = Arc::new(AceSessionStore::with_policy(
            Duration::from_secs(300),
            8,
            Duration::from_secs(60),
        ));
        let source = StreamSession::start(Box::new(PendingSource), 8);
        store.insert_hls_at(
            "startup".into(),
            "id".into(),
            "key".into(),
            now,
            source.subscribe(),
        );
        let request = store.begin_hls_request("id", "startup").unwrap();
        store.purge_expired(now + Duration::from_secs(90));
        assert_eq!(source.subscriber_count(), 1);
        drop(request);
        store.purge_expired(now + Duration::from_secs(120));
        assert_eq!(source.subscriber_count(), 0);
        store.insert_hls_at(
            "absolute".into(),
            "id".into(),
            "key".into(),
            now,
            source.subscribe(),
        );
        let request = store.begin_hls_request("id", "absolute").unwrap();
        store.purge_expired(now + Duration::from_secs(300));
        drop(request);
        assert_eq!(source.subscriber_count(), 0);
        assert_eq!(store.active_count(), 0);
    }

    #[tokio::test]
    async fn reaper_releases_idle_source_without_another_http_request() {
        let store = Arc::new(AceSessionStore::with_policy(
            Duration::from_secs(300),
            8,
            Duration::from_millis(100),
        ));
        let source = StreamSession::start(Box::new(PendingSource), 8);
        let token = store.mint_hls("id".into(), "key".into(), source.subscribe());
        let request = store.begin_hls_request("id", &token).unwrap();
        tokio::task::yield_now().await;
        drop(request);
        tokio::time::timeout(Duration::from_secs(3), async {
            while source.subscriber_count() > 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(store.active_count(), 0);
    }
}
