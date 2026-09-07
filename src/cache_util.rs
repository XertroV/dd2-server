use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

pub struct CacheEntry<T> {
    pub value: T,
    pub first_seen: Instant,
    pub last_checked: Instant,
}

pub struct InnerCache<K, V> {
    pub map: HashMap<K, CacheEntry<V>>,
    pub last_scanned: Instant,
    pub purge_after: Duration,
}

impl<K, V> Default for InnerCache<K, V> {
    fn default() -> Self {
        InnerCache {
            map: HashMap::new(),
            last_scanned: Instant::now(),
            purge_after: Duration::from_secs(600), // Default to 10 min
        }
    }
}

#[derive(Default)]
pub struct AsyncCache<K, V> {
    inner: Arc<RwLock<InnerCache<K, V>>>,
}

impl<K, V> AsyncCache<K, V>
where
    K: std::cmp::Eq + std::hash::Hash + Clone,
    V: Clone + PartialEq,
{
    pub async fn get_or_insert_with<E, F, Fut>(&self, key: &K, ttl: Duration, get_fresh: F) -> Result<(Duration, V), E>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<V, E>>,
    {
        let mut now = Instant::now();
        {
            let inner = self.inner.read().await;
            if let Some(entry) = inner.map.get(&key) {
                if entry.last_checked + ttl > now {
                    return Ok((now - entry.first_seen, entry.value.clone()));
                }
            }
        }

        // Upgrade to write lock to insert or update
        let mut inner = self.inner.write().await;
        if let Some(entry) = inner.map.get(&key) {
            now = Instant::now();
            if entry.last_checked + ttl > now {
                return Ok((now - entry.first_seen, entry.value.clone()));
            }
        }
        let value = get_fresh().await?;
        let cached_at = Instant::now();
        let mut age = Duration::ZERO;

        inner
            .map
            .entry(key.clone())
            .and_modify(|e| {
                if e.value != value {
                    e.value = value.clone();
                    e.first_seen = cached_at;
                }
                e.last_checked = cached_at;
                age = cached_at - e.first_seen;
            })
            .or_insert_with(|| CacheEntry {
                value: value.clone(),
                first_seen: cached_at,
                last_checked: cached_at,
            });

        // RELEASE BEFORE CHECK PURGE
        drop(inner);

        // Purge old entries if needed
        self.check_purge_old().await;

        // Return the value
        Ok((age, value))
    }

    async fn check_purge_old(&self) {
        let now = Instant::now();
        // read lock block
        {
            let inner = self.inner.read().await;
            if now - inner.last_scanned < inner.purge_after {
                return;
            }
        }

        let mut inner = self.inner.write().await;
        // Retain only entries that were checked recently (means someone was in the map)
        let purge_after = inner.purge_after;
        inner.map.retain(|_, entry| now - entry.last_checked < purge_after);
        inner.last_scanned = now;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    #[tokio::test]
    async fn serves_cached_value_within_ttl_and_refreshes_after_it() {
        let cache: AsyncCache<String, i32> = Default::default();
        let calls = Arc::new(AtomicUsize::new(0));
        let ttl = Duration::from_millis(50);

        let fetch = |calls: Arc<AtomicUsize>| {
            move || {
                let calls = calls.clone();
                async move {
                    // Each real fetch returns a different value so the
                    // refresh is observable, and counts how often it ran.
                    Ok::<_, std::convert::Infallible>(10 + calls.fetch_add(1, Ordering::SeqCst) as i32)
                }
            }
        };

        let (age, v) = cache.get_or_insert_with(&"k".to_string(), ttl, fetch(calls.clone())).await.unwrap();
        assert_eq!((age, v), (Duration::ZERO, 10));

        // Within the TTL the second call serves the cached value without fetching.
        let (_, v) = cache.get_or_insert_with(&"k".to_string(), ttl, fetch(calls.clone())).await.unwrap();
        assert_eq!(v, 10);
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        // After the TTL the fetch runs again; a changed value resets the age.
        tokio::time::sleep(ttl + Duration::from_millis(10)).await;
        let (age, v) = cache.get_or_insert_with(&"k".to_string(), ttl, fetch(calls.clone())).await.unwrap();
        assert_eq!(v, 11);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert!(age <= Duration::from_millis(5), "age after a changed-value refresh was {age:?}");
    }
}
