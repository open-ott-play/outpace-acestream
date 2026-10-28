//! Concurrent signed-catalog requests share one cancellable fetch. Completed descriptors,
//! including expiring HLS URLs, are not retained after their callers leave.
use super::{ResolveError, CATALOG_TOTAL_TIMEOUT};
use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use tokio::sync::OnceCell;

type ResultCell = OnceCell<Result<Vec<u8>, ResolveError>>;
const MAX_FLIGHTS: usize = 64;

#[derive(Default)]
pub(super) struct CatalogFlights {
    entries: Mutex<HashMap<(String, String), Weak<ResultCell>>>,
}

impl CatalogFlights {
    pub(super) fn global() -> &'static Self {
        static FLIGHTS: OnceLock<CatalogFlights> = OnceLock::new();
        FLIGHTS.get_or_init(Self::default)
    }

    pub(super) async fn resolve(
        &self,
        parameter: &str,
        value: &str,
        fetch: impl Future<Output = Result<Vec<u8>, ResolveError>>,
    ) -> Result<Vec<u8>, ResolveError> {
        // Waiting behind another initializer spends this caller's existing catalog budget.
        // Taking over a cancelled fetch must not grant a fresh per-host budget to the waiter.
        let deadline = tokio::time::Instant::now() + CATALOG_TOTAL_TIMEOUT;
        // Production keys are a fixed selector and bounded hexadecimal/Base64 identifier.
        if parameter.len() > 16 || value.len() > 64 {
            return Err(ResolveError::Catalog("catalog query key too long"));
        }
        let key = (parameter.to_string(), value.to_string());
        let cell = {
            let mut entries = self.entries.lock().unwrap();
            entries.retain(|_, entry| entry.upgrade().is_some_and(|cell| cell.get().is_none()));
            if let Some(cell) = entries.get(&key).and_then(Weak::upgrade) {
                cell
            } else {
                if entries.len() >= MAX_FLIGHTS {
                    return Err(ResolveError::Catalog(
                        "too many concurrent catalog resolutions",
                    ));
                }
                let cell = Arc::new(OnceCell::new());
                entries.insert(key, Arc::downgrade(&cell));
                cell
            }
        };
        // No spawned task owns `fetch`: cancellation drops the initializer. Another waiting
        // caller can initialize, while dropping every caller cancels all work for this key.
        tokio::time::timeout_at(deadline, cell.get_or_init(|| fetch))
            .await
            .map_err(|_| ResolveError::Catalog("catalog timeout"))?
            .clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::Notify;

    #[tokio::test]
    async fn concurrent_callers_share_but_later_request_fetches_fresh() {
        let flights = CatalogFlights::default();
        let calls = AtomicUsize::new(0);
        let release = Notify::new();
        let (first, second, _) = tokio::join!(
            flights.resolve("pid", "synthetic", async {
                calls.fetch_add(1, Ordering::SeqCst);
                release.notified().await;
                Ok(vec![1])
            }),
            flights.resolve("pid", "synthetic", async {
                panic!("overlapping request must share the existing fetch")
            }),
            async { release.notify_one() },
        );
        assert_eq!(first, Ok(vec![1]));
        assert_eq!(second, first);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            flights
                .resolve("pid", "synthetic", async { Ok(vec![2]) })
                .await,
            Ok(vec![2])
        );
    }

    #[tokio::test]
    async fn cancellation_drops_fetch_and_waiter_can_take_over() {
        let flights = CatalogFlights::default();
        let dropped = AtomicUsize::new(0);
        struct DropCount<'a>(&'a AtomicUsize);
        impl Drop for DropCount<'_> {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let mut first = Box::pin(flights.resolve("pid", "synthetic", async {
            let _guard = DropCount(&dropped);
            std::future::pending().await
        }));
        assert!(
            std::future::poll_fn(|cx| std::task::Poll::Ready(first.as_mut().poll(cx).is_pending()))
                .await
        );
        let mut second = Box::pin(flights.resolve("pid", "synthetic", async { Ok(vec![3]) }));
        assert!(
            std::future::poll_fn(|cx| std::task::Poll::Ready(
                second.as_mut().poll(cx).is_pending()
            ))
            .await
        );
        drop(first);
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
        assert_eq!(second.await, Ok(vec![3]));
        assert_eq!(
            flights
                .resolve("pid", "synthetic", async { Ok(vec![4]) })
                .await,
            Ok(vec![4])
        );
    }

    #[tokio::test]
    async fn active_query_capacity_is_bounded() {
        let flights = CatalogFlights::default();
        let mut active = Vec::new();
        for n in 0..MAX_FLIGHTS {
            let flights = &flights;
            let mut future = Box::pin(async move {
                flights
                    .resolve("pid", &n.to_string(), std::future::pending())
                    .await
            });
            assert!(
                std::future::poll_fn(|cx| std::task::Poll::Ready(
                    future.as_mut().poll(cx).is_pending()
                ))
                .await
            );
            active.push(future);
        }
        assert!(matches!(
            flights
                .resolve("infohash", "0", async { Ok(vec![9]) })
                .await,
            Err(ResolveError::Catalog(
                "too many concurrent catalog resolutions"
            ))
        ));
        drop(active);
        assert_eq!(
            flights
                .resolve("infohash", "0", async { Ok(vec![9]) })
                .await,
            Ok(vec![9])
        );
    }

    #[tokio::test]
    async fn selector_namespaces_and_unrelated_work_are_independent() {
        let flights = CatalogFlights::default();
        let release = Notify::new();
        let calls = AtomicUsize::new(0);
        let (pid, hash) = tokio::time::timeout(std::time::Duration::from_secs(1), async {
            tokio::join!(
                flights.resolve("pid", "same-value", async {
                    calls.fetch_add(1, Ordering::SeqCst);
                    release.notified().await;
                    Ok(vec![1])
                }),
                async {
                    let result = flights
                        .resolve("infohash", "same-value", async {
                            calls.fetch_add(1, Ordering::SeqCst);
                            Ok(vec![2])
                        })
                        .await;
                    release.notify_one();
                    result
                },
            )
        })
        .await
        .expect("one pending selector must not block another namespace");
        assert_eq!(pid, Ok(vec![1]));
        assert_eq!(hash, Ok(vec![2]));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn failed_fetch_is_not_retained_for_later_retry() {
        let flights = CatalogFlights::default();
        assert_eq!(
            flights
                .resolve("pid", "synthetic", async {
                    Err(ResolveError::Catalog("synthetic failure"))
                })
                .await,
            Err(ResolveError::Catalog("synthetic failure"))
        );
        assert_eq!(
            flights
                .resolve("pid", "synthetic", async { Ok(vec![5]) })
                .await,
            Ok(vec![5])
        );
    }

    #[tokio::test]
    async fn sole_cancelled_caller_leaves_no_work_or_retained_result() {
        let flights = CatalogFlights::default();
        let dropped = AtomicUsize::new(0);
        struct DropCount<'a>(&'a AtomicUsize);
        impl Drop for DropCount<'_> {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let mut request = Box::pin(flights.resolve("pid", "synthetic", async {
            let _guard = DropCount(&dropped);
            std::future::pending().await
        }));
        assert!(
            std::future::poll_fn(|cx| std::task::Poll::Ready(
                request.as_mut().poll(cx).is_pending()
            ))
            .await
        );
        drop(request);
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
        assert!(flights
            .entries
            .lock()
            .unwrap()
            .values()
            .all(|entry| entry.strong_count() == 0));
        assert_eq!(
            flights
                .resolve("pid", "synthetic", async { Ok(vec![6]) })
                .await,
            Ok(vec![6])
        );
    }

    #[tokio::test]
    async fn new_request_does_not_reuse_completed_result_held_by_a_slow_waiter() {
        let flights = CatalogFlights::default();
        let release = Notify::new();
        let mut first = Box::pin(flights.resolve("pid", "synthetic", async {
            release.notified().await;
            Ok(vec![1])
        }));
        assert!(
            std::future::poll_fn(|cx| std::task::Poll::Ready(first.as_mut().poll(cx).is_pending()))
                .await
        );
        let mut slow = Box::pin(flights.resolve("pid", "synthetic", async {
            panic!("already waiting caller must share the original fetch")
        }));
        assert!(
            std::future::poll_fn(|cx| std::task::Poll::Ready(slow.as_mut().poll(cx).is_pending()))
                .await
        );
        release.notify_one();
        assert_eq!(first.await, Ok(vec![1]));
        assert_eq!(
            flights
                .resolve("pid", "synthetic", async { Ok(vec![2]) })
                .await,
            Ok(vec![2])
        );
        assert_eq!(slow.await, Ok(vec![1]));
    }

    #[tokio::test(start_paused = true)]
    async fn cancelled_owner_takeover_keeps_the_waiters_original_deadline() {
        let flights = CatalogFlights::default();
        let started = AtomicUsize::new(0);
        let dropped = AtomicUsize::new(0);
        struct DropCount<'a>(&'a AtomicUsize);
        impl Drop for DropCount<'_> {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let began = tokio::time::Instant::now();
        let mut owner = Box::pin(flights.resolve("pid", "synthetic", async {
            let _guard = DropCount(&dropped);
            std::future::pending().await
        }));
        assert!(
            std::future::poll_fn(|cx| std::task::Poll::Ready(owner.as_mut().poll(cx).is_pending()))
                .await
        );
        let mut waiter = Box::pin(flights.resolve("pid", "synthetic", async {
            started.fetch_add(1, Ordering::SeqCst);
            let _guard = DropCount(&dropped);
            std::future::pending().await
        }));
        assert!(
            std::future::poll_fn(|cx| std::task::Poll::Ready(
                waiter.as_mut().poll(cx).is_pending()
            ))
            .await
        );
        let remaining = std::time::Duration::from_secs(1);
        tokio::time::advance(CATALOG_TOTAL_TIMEOUT - remaining).await;
        drop(owner);
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
        assert!(
            std::future::poll_fn(|cx| std::task::Poll::Ready(
                waiter.as_mut().poll(cx).is_pending()
            ))
            .await
        );
        assert_eq!(
            started.load(Ordering::SeqCst),
            1,
            "waiting caller takes over the cancelled initializer"
        );
        tokio::time::advance(remaining).await;
        assert_eq!(waiter.await, Err(ResolveError::Catalog("catalog timeout")));
        assert_eq!(
            began.elapsed(),
            CATALOG_TOTAL_TIMEOUT,
            "wait plus takeover shares one absolute budget"
        );
        assert_eq!(
            dropped.load(Ordering::SeqCst),
            2,
            "timing out drops the replacement fetch"
        );
        assert!(flights
            .entries
            .lock()
            .unwrap()
            .values()
            .all(|entry| entry.strong_count() == 0));
        assert_eq!(
            flights
                .resolve("pid", "synthetic", async { Ok(vec![7]) })
                .await,
            Ok(vec![7])
        );
    }
}
