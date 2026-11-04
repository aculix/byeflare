use crate::domain::{Cleared, NamedCookie, SolveError, Target};
use crate::solver::Solver;
use lru::LruCache;
use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, MutexGuard};

struct Entry {
    cookies: Vec<NamedCookie>,
    expires: Instant,
}

pub struct Cache {
    entries: Mutex<LruCache<String, Entry>>,
    gates: Mutex<HashMap<String, Arc<Mutex<()>>>>,
}

impl Cache {
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: Mutex::new(LruCache::new(NonZeroUsize::new(capacity).unwrap())),
            gates: Mutex::new(HashMap::new()),
        }
    }

    pub async fn get(&self, key: &str) -> Option<Vec<NamedCookie>> {
        let mut entries = self.entries.lock().await;
        match entries.get(key) {
            Some(entry) if entry.expires > Instant::now() => Some(entry.cookies.clone()),
            Some(_) => {
                entries.pop(key);
                None
            }
            None => None,
        }
    }

    pub async fn insert(&self, key: String, cookies: Vec<NamedCookie>, expires: Instant) {
        self.entries.lock().await.put(key, Entry { cookies, expires });
    }

    pub async fn invalidate(&self, key: &str) {
        self.entries.lock().await.pop(key);
    }

    async fn gate(&self, key: &str) -> Arc<Mutex<()>> {
        let mut gates = self.gates.lock().await;
        gates.entry(key.to_string()).or_default().clone()
    }

    /// Returns cookies, solving once for all concurrent callers. The shared solve is
    /// bounded by `max_solve`; the caller's own deadline is not used here.
    pub async fn get_or_solve(
        &self,
        solver: &dyn Solver,
        key: &str,
        target: Target,
        expires: Instant,
        max_solve: Duration,
    ) -> Result<Vec<NamedCookie>, SolveError> {
        if let Some(cookies) = self.get(key).await {
            return Ok(cookies);
        }
        let gate = self.gate(key).await;
        let _guard: MutexGuard<'_, ()> = gate.lock().await;
        if let Some(cookies) = self.get(key).await {
            return Ok(cookies);
        }
        let solved = tokio::time::timeout(max_solve, solver.clear(&target)).await;
        match solved {
            Ok(Ok(Cleared { cookies, .. })) => {
                self.insert(key.to_string(), cookies.clone(), expires).await;
                Ok(cookies)
            }
            Ok(Err(error)) => Err(error),
            Err(_) => Err(SolveError::Network("solve timed out".into())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::solver::Solver;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Counting(Arc<AtomicUsize>, Result<Cleared, SolveError>);

    #[async_trait]
    impl Solver for Counting {
        async fn clear(&self, _target: &Target) -> Result<Cleared, SolveError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            match &self.1 {
                Ok(cleared) => Ok(Cleared {
                    url: cleared.url.clone(),
                    status: cleared.status,
                    body: cleared.body.clone(),
                    cookies: cleared.cookies.clone(),
                }),
                Err(error) => Err(error.clone()),
            }
        }
    }

    fn cookies() -> Vec<NamedCookie> {
        vec![NamedCookie {
            name: "cf_clearance".into(),
            value: "v".into(),
            domain: "host".into(),
            path: "/".into(),
            expires: None,
            http_only: true,
            secure: true,
        }]
    }

    fn solved() -> Cleared {
        Cleared {
            url: "https://host/".into(),
            status: 200,
            body: "ok".into(),
            cookies: cookies(),
        }
    }

    fn target() -> Target {
        Target {
            url: "https://host/".into(),
            proxy: None,
            deadline: Instant::now() + Duration::from_secs(30),
        }
    }

    fn later() -> Instant {
        Instant::now() + Duration::from_secs(60)
    }

    #[tokio::test]
    async fn insert_then_get_returns_cookies() {
        let cache = Cache::new(8);
        cache.insert("k".into(), cookies(), later()).await;
        assert_eq!(cache.get("k").await, Some(cookies()));
    }

    #[tokio::test]
    async fn expired_entries_miss() {
        let cache = Cache::new(8);
        cache
            .insert("k".into(), cookies(), Instant::now() - Duration::from_secs(1))
            .await;
        assert_eq!(cache.get("k").await, None);
    }

    #[tokio::test]
    async fn the_cap_evicts_least_recent() {
        let cache = Cache::new(2);
        cache.insert("a".into(), cookies(), later()).await;
        cache.insert("b".into(), cookies(), later()).await;
        cache.get("a").await;
        cache.insert("c".into(), cookies(), later()).await;
        assert!(cache.get("b").await.is_none());
        assert!(cache.get("a").await.is_some());
    }

    #[tokio::test]
    async fn invalidate_removes() {
        let cache = Cache::new(8);
        cache.insert("k".into(), cookies(), later()).await;
        cache.invalidate("k").await;
        assert!(cache.get("k").await.is_none());
    }

    #[tokio::test]
    async fn waiters_share_one_solve() {
        let calls = Arc::new(AtomicUsize::new(0));
        let solver = Counting(calls.clone(), Ok(solved()));
        let cache = Cache::new(8);
        let (a, b) = tokio::join!(
            cache.get_or_solve(&solver, "k", target(), later(), Duration::from_secs(5)),
            cache.get_or_solve(&solver, "k", target(), later(), Duration::from_secs(5)),
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(a.unwrap(), b.unwrap());
    }

    #[tokio::test]
    async fn a_failure_reaches_the_caller() {
        let calls = Arc::new(AtomicUsize::new(0));
        let solver = Counting(calls.clone(), Err(SolveError::Unsupported("new build".into())));
        let cache = Cache::new(8);
        let error = cache
            .get_or_solve(&solver, "k", target(), later(), Duration::from_secs(5))
            .await
            .unwrap_err();
        assert_eq!(error, SolveError::Unsupported("new build".into()));
    }

    #[tokio::test]
    async fn the_shared_solve_is_bounded_by_max_solve() {
        struct Slow;
        #[async_trait]
        impl Solver for Slow {
            async fn clear(&self, _target: &Target) -> Result<Cleared, SolveError> {
                tokio::time::sleep(Duration::from_secs(10)).await;
                Ok(solved())
            }
        }
        let cache = Cache::new(8);
        let error = cache
            .get_or_solve(&Slow, "k", target(), later(), Duration::from_millis(20))
            .await
            .unwrap_err();
        assert!(matches!(error, SolveError::Network(_)));
    }
}
