use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use dashmap::DashMap;
use parking_lot::{Mutex, RwLock};
use reqwest::{Method, Response, StatusCode, header};
use serde::Serialize;
use tokio::sync::Mutex as AsyncMutex;
use url::Url;
use uuid::Uuid;

use crate::{
    config::Config,
    error::{Error, Result},
    gst::{MediaInfo, ProbeRequest, probe},
    session::{Source, validate_source},
};

#[derive(Debug)]
pub struct RegisteredSource {
    pub id: Uuid,
    pub original: Source,
    pub media: MediaInfo,
    resolved: RwLock<ResolvedSource>,
    references: AtomicUsize,
    touched: Mutex<Instant>,
    refresh: AsyncMutex<()>,
    rate_limited_until: Mutex<Option<Instant>>,
}

#[derive(Clone, Debug)]
struct ResolvedSource {
    url: Url,
    headers: BTreeMap<String, String>,
}

impl RegisteredSource {
    pub fn touch(&self) {
        *self.touched.lock() = Instant::now();
    }

    fn inactive_for(&self) -> Duration {
        self.touched.lock().elapsed()
    }

    pub fn resolved(&self) -> (Url, BTreeMap<String, String>) {
        let resolved = self.resolved.read();
        (resolved.url.clone(), resolved.headers.clone())
    }

    fn acquire(&self) {
        self.references.fetch_add(1, Ordering::Relaxed);
        self.touch();
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct SourceView {
    pub id: Uuid,
    pub media: MediaInfo,
    pub relay_url: String,
}

#[derive(Default)]
struct SourceMetrics {
    registrations: AtomicU64,
    deduplicated_registrations: AtomicU64,
    resolver_requests: AtomicU64,
    relay_requests: AtomicU64,
    cdn_range_requests: AtomicU64,
    rate_limited: AtomicU64,
    refreshes: AtomicU64,
}

#[derive(Clone, Debug, Serialize)]
pub struct SourceMetricsSnapshot {
    pub source_registrations: u64,
    pub deduplicated_source_registrations: u64,
    pub resolver_requests: u64,
    pub relay_requests: u64,
    pub cdn_range_requests: u64,
    pub source_rate_limited: u64,
    pub source_refreshes: u64,
}

#[derive(Clone)]
pub struct SourceManager {
    config: Config,
    client: reqwest::Client,
    sources: Arc<DashMap<Uuid, Arc<RegisteredSource>>>,
    fingerprints: Arc<DashMap<String, Uuid>>,
    registration_locks: Arc<DashMap<String, Arc<AsyncMutex<()>>>>,
    rate_limits: Arc<DashMap<String, Instant>>,
    metrics: Arc<SourceMetrics>,
}

impl SourceManager {
    /// Creates an empty source registry and pooled HTTP client.
    ///
    /// # Errors
    ///
    /// Returns an error when the HTTP client cannot be constructed.
    pub fn new(config: Config) -> Result<Self> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::limited(10))
            .pool_idle_timeout(Duration::from_secs(90))
            .pool_max_idle_per_host(8)
            .build()
            .map_err(|error| Error::Task(format!("HTTP client setup failed: {error}")))?;
        Ok(Self {
            config,
            client,
            sources: Arc::new(DashMap::new()),
            fingerprints: Arc::new(DashMap::new()),
            registration_locks: Arc::new(DashMap::new()),
            rate_limits: Arc::new(DashMap::new()),
            metrics: Arc::new(SourceMetrics::default()),
        })
    }

    /// Resolves, probes, and deduplicates a source registration.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid URLs, rate limits, resolver failures, or
    /// media discovery failures.
    pub async fn register(&self, source: Source) -> Result<Arc<RegisteredSource>> {
        validate_source(&source)?;
        self.evict_expired();
        self.evict_over_capacity();
        let fingerprint = source_fingerprint(&source);
        let registration_lock = self
            .registration_locks
            .entry(fingerprint.clone())
            .or_insert_with(|| Arc::new(AsyncMutex::new(())))
            .clone();
        let _registration_guard = registration_lock.lock().await;
        if let Some(deadline) = self
            .rate_limits
            .get(&fingerprint)
            .map(|entry| *entry.value())
        {
            if deadline > Instant::now() {
                return Err(Error::SourceRateLimited {
                    retry_after_seconds: Some(
                        deadline.saturating_duration_since(Instant::now()).as_secs(),
                    ),
                });
            }
            self.rate_limits.remove(&fingerprint);
        }
        if let Some(id) = self
            .fingerprints
            .get(&fingerprint)
            .map(|entry| *entry.value())
        {
            if let Some(existing) = self.sources.get(&id).map(|entry| Arc::clone(entry.value())) {
                existing.touch();
                self.metrics
                    .deduplicated_registrations
                    .fetch_add(1, Ordering::Relaxed);
                return Ok(existing);
            }
            self.fingerprints.remove(&fingerprint);
        }

        let resolved = match self.resolve(&source).await {
            Ok(resolved) => resolved,
            Err(Error::SourceRateLimited {
                retry_after_seconds,
            }) => {
                self.rate_limits.insert(
                    fingerprint.clone(),
                    Instant::now() + Duration::from_secs(retry_after_seconds.unwrap_or(60)),
                );
                return Err(Error::SourceRateLimited {
                    retry_after_seconds,
                });
            }
            Err(error) => return Err(error),
        };
        let probe_request = ProbeRequest {
            url: resolved.url.clone(),
            headers: resolved.headers.clone(),
            timeout: self.config.probe_timeout(),
        };
        let media = tokio::task::spawn_blocking(move || probe(&probe_request))
            .await
            .map_err(|error| Error::Task(error.to_string()))?
            .map_err(|error| match error {
                Error::Discovery(_) => {
                    Error::Discovery("registered source could not be inspected".to_owned())
                }
                other => other,
            })?;
        let id = Uuid::new_v4();
        let registered = Arc::new(RegisteredSource {
            id,
            original: source,
            media,
            resolved: RwLock::new(resolved),
            references: AtomicUsize::new(0),
            touched: Mutex::new(Instant::now()),
            refresh: AsyncMutex::new(()),
            rate_limited_until: Mutex::new(None),
        });
        self.sources.insert(id, Arc::clone(&registered));
        self.fingerprints.insert(fingerprint, id);
        self.metrics.registrations.fetch_add(1, Ordering::Relaxed);
        Ok(registered)
    }

    /// Acquires a session reference to a registered source.
    ///
    /// # Errors
    ///
    /// Returns an error when the source ID is unknown.
    pub fn acquire(&self, id: Uuid) -> Result<Arc<RegisteredSource>> {
        let source = self.get(id)?;
        source.acquire();
        Ok(source)
    }

    /// Retrieves and touches a registered source.
    ///
    /// # Errors
    ///
    /// Returns an error when the source ID is unknown.
    pub fn get(&self, id: Uuid) -> Result<Arc<RegisteredSource>> {
        let source = self
            .sources
            .get(&id)
            .map(|entry| Arc::clone(entry.value()))
            .ok_or(Error::SourceNotFound(id))?;
        source.touch();
        Ok(source)
    }

    /// Releases an external source handle when no session owns it.
    ///
    /// # Errors
    ///
    /// Returns an error when the source ID is unknown.
    pub fn release(&self, id: Uuid) -> Result<()> {
        let Some(source) = self.sources.get(&id).map(|entry| Arc::clone(entry.value())) else {
            return Err(Error::SourceNotFound(id));
        };
        if source.references.load(Ordering::Acquire) == 0 {
            self.sources.remove(&id);
            self.remove_indexes(&source);
        } else {
            source.touch();
        }
        Ok(())
    }

    /// Releases one session reference.
    ///
    /// # Errors
    ///
    /// Returns an error for an unknown source or reference-count underflow.
    pub fn release_session(&self, id: Uuid) -> Result<()> {
        let source = self.get(id)?;
        let previous = source.references.fetch_sub(1, Ordering::AcqRel);
        if previous == 0 {
            source.references.store(0, Ordering::Release);
            return Err(Error::Task("source reference count underflow".to_owned()));
        }
        Ok(())
    }

    /// Forwards one browser range request through the pinned CDN endpoint.
    ///
    /// # Errors
    ///
    /// Returns an error when the source is rate limited, transport fails, or a
    /// single-flight refresh cannot renew an expired endpoint.
    pub async fn relay(
        &self,
        source: &Arc<RegisteredSource>,
        method: Method,
        range: Option<&str>,
        if_range: Option<&str>,
    ) -> Result<Response> {
        Self::assert_not_rate_limited(source)?;
        self.metrics.relay_requests.fetch_add(1, Ordering::Relaxed);
        if range.is_some() {
            self.metrics
                .cdn_range_requests
                .fetch_add(1, Ordering::Relaxed);
        }
        let attempted_url = source.resolved.read().url.clone();
        let response = self
            .send_resolved(source, method.clone(), range, if_range)
            .await?;
        if !matches!(
            response.status(),
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
        ) {
            return self.check_rate_limit(source, response);
        }

        let _refresh_guard = source.refresh.lock().await;
        Self::assert_not_rate_limited(source)?;
        let endpoint_is_stale = source.resolved.read().url == attempted_url;
        if endpoint_is_stale {
            let refreshed = self.resolve(&source.original).await?;
            *source.resolved.write() = refreshed;
            self.metrics.refreshes.fetch_add(1, Ordering::Relaxed);
        }
        let response = self.send_resolved(source, method, range, if_range).await?;
        self.check_rate_limit(source, response)
    }

    #[must_use]
    pub fn metrics(&self) -> SourceMetricsSnapshot {
        SourceMetricsSnapshot {
            source_registrations: self.metrics.registrations.load(Ordering::Relaxed),
            deduplicated_source_registrations: self
                .metrics
                .deduplicated_registrations
                .load(Ordering::Relaxed),
            resolver_requests: self.metrics.resolver_requests.load(Ordering::Relaxed),
            relay_requests: self.metrics.relay_requests.load(Ordering::Relaxed),
            cdn_range_requests: self.metrics.cdn_range_requests.load(Ordering::Relaxed),
            source_rate_limited: self.metrics.rate_limited.load(Ordering::Relaxed),
            source_refreshes: self.metrics.refreshes.load(Ordering::Relaxed),
        }
    }

    async fn resolve(&self, source: &Source) -> Result<ResolvedSource> {
        if source.url.scheme() == "file" {
            return Ok(ResolvedSource {
                url: source.url.clone(),
                headers: source.headers.clone(),
            });
        }
        self.metrics
            .resolver_requests
            .fetch_add(1, Ordering::Relaxed);
        let mut request = self
            .client
            .get(source.url.clone())
            .header(header::RANGE, "bytes=0-0");
        for (name, value) in &source.headers {
            request = request.header(name, value);
        }
        let response = request.send().await.map_err(|error| {
            Error::Discovery(format!("source resolution failed: {}", error.without_url()))
        })?;
        if response.status() == StatusCode::TOO_MANY_REQUESTS {
            self.metrics.rate_limited.fetch_add(1, Ordering::Relaxed);
            return Err(rate_limit_error(&response));
        }
        if !response.status().is_success() {
            return Err(Error::Discovery(format!(
                "source resolution returned HTTP {}",
                response.status()
            )));
        }
        if response.status() != StatusCode::PARTIAL_CONTENT
            && response.content_length().is_some_and(|length| length > 1)
        {
            return Err(Error::InvalidSource(
                "remote source does not honor byte-range requests".to_owned(),
            ));
        }
        let final_url = response.url().clone();
        let mut headers = source.headers.clone();
        if final_url.origin() != source.url.origin() {
            headers.retain(|name, _| {
                !name.eq_ignore_ascii_case("authorization") && !name.eq_ignore_ascii_case("cookie")
            });
        }
        Ok(ResolvedSource {
            url: final_url,
            headers,
        })
    }

    async fn send_resolved(
        &self,
        source: &RegisteredSource,
        method: Method,
        range: Option<&str>,
        if_range: Option<&str>,
    ) -> Result<Response> {
        let mut request = {
            let resolved = source.resolved.read();
            let mut request = self.client.request(method, resolved.url.clone());
            for (name, value) in &resolved.headers {
                request = request.header(name, value);
            }
            drop(resolved);
            request
        };
        if let Some(range) = range {
            request = request.header(header::RANGE, range);
        }
        if let Some(if_range) = if_range {
            request = request.header(header::IF_RANGE, if_range);
        }
        request.send().await.map_err(|error| {
            Error::Pipeline(format!("source relay failed: {}", error.without_url()))
        })
    }

    fn check_rate_limit(&self, source: &RegisteredSource, response: Response) -> Result<Response> {
        if response.status() != StatusCode::TOO_MANY_REQUESTS {
            return Ok(response);
        }
        let error = rate_limit_error(&response);
        if let Error::SourceRateLimited {
            retry_after_seconds,
        } = error
        {
            let duration = Duration::from_secs(retry_after_seconds.unwrap_or(60));
            *source.rate_limited_until.lock() = Some(Instant::now() + duration);
            self.metrics.rate_limited.fetch_add(1, Ordering::Relaxed);
            return Err(Error::SourceRateLimited {
                retry_after_seconds,
            });
        }
        Err(error)
    }

    fn assert_not_rate_limited(source: &RegisteredSource) -> Result<()> {
        let mut until = source.rate_limited_until.lock();
        if let Some(deadline) = *until {
            if deadline > Instant::now() {
                return Err(Error::SourceRateLimited {
                    retry_after_seconds: Some(
                        deadline.saturating_duration_since(Instant::now()).as_secs(),
                    ),
                });
            }
            *until = None;
        }
        drop(until);
        Ok(())
    }

    fn evict_expired(&self) {
        let ttl = self.config.session_ttl();
        let expired = self
            .sources
            .iter()
            .filter(|entry| {
                entry.value().references.load(Ordering::Relaxed) == 0
                    && entry.value().inactive_for() >= ttl
            })
            .map(|entry| *entry.key())
            .collect::<Vec<_>>();
        for id in expired {
            if let Some((_, source)) = self.sources.remove(&id) {
                self.remove_indexes(&source);
            }
        }
    }

    fn evict_over_capacity(&self) {
        if self.sources.len() < self.config.max_sessions.max(1) {
            return;
        }
        let oldest = self
            .sources
            .iter()
            .filter(|entry| entry.value().references.load(Ordering::Relaxed) == 0)
            .max_by_key(|entry| entry.value().inactive_for())
            .map(|entry| *entry.key());
        if let Some(id) = oldest
            && let Some((_, source)) = self.sources.remove(&id)
        {
            self.remove_indexes(&source);
        }
    }

    fn remove_indexes(&self, source: &RegisteredSource) {
        let fingerprint = source_fingerprint(&source.original);
        self.fingerprints.remove(&fingerprint);
        self.registration_locks.remove(&fingerprint);
    }
}

fn source_fingerprint(source: &Source) -> String {
    let mut value = source.url.as_str().to_owned();
    for (name, header) in &source.headers {
        value.push('\n');
        value.push_str(name);
        value.push(':');
        value.push_str(header);
    }
    value
}

fn rate_limit_error(response: &Response) -> Error {
    let retry_after_seconds = response
        .headers()
        .get(header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    Error::SourceRateLimited {
        retry_after_seconds,
    }
}
