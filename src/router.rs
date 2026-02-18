use crate::lb;
use std::borrow::Cow;
use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::collections::{HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use arc_swap::ArcSwap;
use http::header;
use http::{StatusCode, Uri};
use pingora::http::RequestHeader;
use pingora::http::ResponseHeader;
use pingora::lb::{LoadBalancer, selection::RoundRobin};
use pingora::modules::http::HttpModules;
use pingora::proxy::{FailToProxy, ProxyHttp, Session};
use pingora::upstreams::peer::HttpPeer;
use pingora::{Error, ErrorType, Result};

#[derive(Clone, Default)]
pub struct RouterConfig {
    pub subdomain_routes: HashMap<String, String>,
    pub path_routes: HashMap<String, String>,
    pub default_app: Option<String>,
    pub apps: HashMap<String, AppPools>,
}

#[derive(Clone)]
pub struct AppPools {
    pub http1: Option<PoolConfig>,
    pub ws: Option<PoolConfig>,
    pub http2: Option<PoolConfig>,
}

#[derive(Clone)]
pub struct PoolConfig {
    pub lb: Arc<LoadBalancer<RoundRobin>>,
    pub secure: bool,
    pub verify_hostname: String,
    pub backends: Arc<HashSet<String>>,
}

/// Pre-resolved pool information cached in context to avoid repeated lookups.
#[derive(Clone)]
pub struct ResolvedPool {
    pub lb: Arc<LoadBalancer<RoundRobin>>,
    pub secure: bool,
    pub verify_hostname: String,
    pub is_http2: bool,
    pub transport: lb::TransportKind,
    pub backends: Arc<HashSet<String>>,
}

#[derive(Clone)]
pub struct StickySessionConfig {
    pub enabled: bool,
    pub cookie_name: String,
    pub header_name: String,
    pub ttl: Duration,
    pub max_entries: usize,
    pub cookie_secure: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct AffinityMapKey {
    app_name: String,
    transport: lb::TransportKind,
    affinity_key: String,
}

#[derive(Debug, Clone)]
struct AffinityEntry {
    backend: String,
    expires_at: Instant,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct EvictionItem {
    expires_at: Instant,
    sequence: u64,
    shard_index: usize,
    key: AffinityMapKey,
}

impl PartialOrd for EvictionItem {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for EvictionItem {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.expires_at
            .cmp(&other.expires_at)
            .then_with(|| self.sequence.cmp(&other.sequence))
    }
}

struct StickyShard {
    entries: Mutex<HashMap<AffinityMapKey, AffinityEntry>>,
    op_counter: AtomicU64,
}

struct StickySessionState {
    config: StickySessionConfig,
    shards: Vec<StickyShard>,
    eviction_index: Mutex<BinaryHeap<Reverse<EvictionItem>>>,
    key_counter: AtomicU64,
    eviction_counter: AtomicU64,
}

const STICKY_SHARD_COUNT: usize = 64;
const MAX_AFFINITY_KEY_LEN: usize = 256;

#[allow(dead_code)]
pub struct Router {
    config: ArcSwap<RouterConfig>,
    sticky: Option<Arc<StickySessionState>>,
}

impl Router {
    #[allow(dead_code)]
    pub fn new(config: RouterConfig, sticky_config: StickySessionConfig) -> Self {
        let sticky = if sticky_config.enabled {
            let mut shards = Vec::with_capacity(STICKY_SHARD_COUNT);
            for _ in 0..STICKY_SHARD_COUNT {
                shards.push(StickyShard {
                    entries: Mutex::new(HashMap::new()),
                    op_counter: AtomicU64::new(0),
                });
            }

            Some(Arc::new(StickySessionState {
                config: sticky_config,
                shards,
                eviction_index: Mutex::new(BinaryHeap::new()),
                key_counter: AtomicU64::new(1),
                eviction_counter: AtomicU64::new(1),
            }))
        } else {
            None
        };

        Self {
            config: ArcSwap::from_pointee(config),
            sticky,
        }
    }

    #[allow(dead_code)]
    pub fn update(&self, config: RouterConfig) {
        self.config.store(Arc::new(config));
    }
}

#[derive(Clone)]
pub struct SharedRouter(pub Arc<Router>);

#[derive(Clone, Default)]
pub struct RouterCtx {
    pub app_name: Option<String>,
    pub path_rewrite_segment: Option<String>,
    pub is_upgrade: bool,
    /// Cached pool resolution from request_filter to avoid re-lookup in upstream_peer.
    pub resolved_pool: Option<ResolvedPool>,
    pub selected_transport: Option<lb::TransportKind>,
    pub affinity_key: Option<String>,
    pub should_set_cookie: bool,
    pub sticky_retry_attempted: bool,
    pub failure_hint: Option<FailureHint>,
}

#[derive(Clone, Copy)]
pub enum FailureHint {
    MissingApplication,
    MissingWsPool,
    MissingUpstreamPool,
    UnhealthyUpstream,
}

impl SharedRouter {
    pub fn new(router: Arc<Router>) -> Self {
        Self(router)
    }
}

#[async_trait::async_trait]
impl ProxyHttp for SharedRouter {
    type CTX = RouterCtx;

    fn new_ctx(&self) -> Self::CTX {
        RouterCtx::default()
    }

    fn init_downstream_modules(&self, modules: &mut HttpModules) {
        // Keep Pingora's default behavior (disabled compression) explicit here so we
        // have a clear extension point for adding static downstream modules later.
        modules
            .add_module(pingora::modules::http::compression::ResponseCompressionBuilder::enable(0));
    }

    async fn request_filter(&self, session: &mut Session, ctx: &mut Self::CTX) -> Result<bool> {
        let config = self.0.config.load();

        // TODO(vNext): Deterministic downstream error mapping.
        // Today, a number of routing/upstream-selection failures bubble up as Pingora internal errors,
        // which typically become HTTP 500 responses but without a fully controlled body/headers.
        // Decide and implement a single, explicit downstream error policy for at least:
        // - no application matched (no subdomain/path/default)
        // - matched app has no pools configured
        // - no pools available for request type (e.g. upgrade requires http1)
        // - no healthy upstreams available
        // Acceptance: response status/body/headers are stable across versions and covered by tests.

        let host = extract_host(session);
        let path_first_segment = extract_first_path_segment(session);
        let is_upgrade = session.is_upgrade_req();

        let mut app_name: Option<String> = None;
        let mut rewrite_segment: Option<String> = None;

        if let Some(host) = host.as_deref() {
            app_name = config.subdomain_routes.get(host).cloned();
        }

        if app_name.is_none()
            && let Some(seg) = path_first_segment
            && let Some(app) = config.path_routes.get(seg).cloned()
        {
            app_name = Some(app);
            rewrite_segment = Some(seg.to_string());
        }

        if app_name.is_none() {
            app_name = config.default_app.clone();
        }

        if app_name.is_none() {
            ctx.failure_hint = Some(FailureHint::MissingApplication);
            return Err(Error::explain(
                ErrorType::HTTPStatus(StatusCode::NOT_FOUND.as_u16()),
                "no application matched",
            ));
        }

        ctx.app_name = app_name;
        ctx.path_rewrite_segment = rewrite_segment;
        ctx.is_upgrade = is_upgrade;

        // Pre-resolve the pool to avoid repeated HashMap lookups in upstream_peer.
        if let Some(ref app_name) = ctx.app_name
            && let Some(pools) = config.apps.get(app_name)
        {
            let (pool, is_http2) = if is_upgrade {
                (pools.ws.as_ref(), false)
            } else if let Some(p) = pools.http2.as_ref() {
                (Some(p), true)
            } else {
                (pools.http1.as_ref(), false)
            };

            if let Some(pool) = pool {
                ctx.resolved_pool = Some(ResolvedPool {
                    lb: Arc::clone(&pool.lb),
                    secure: pool.secure,
                    verify_hostname: pool.verify_hostname.clone(),
                    is_http2,
                    transport: if is_upgrade {
                        lb::TransportKind::Ws
                    } else if is_http2 {
                        lb::TransportKind::Http2
                    } else {
                        lb::TransportKind::Http1
                    },
                    backends: Arc::clone(&pool.backends),
                });
            }
        }

        if let (Some(sticky), Some(_app_name), Some(_resolved)) = (
            self.0.sticky.as_ref(),
            ctx.app_name.as_ref(),
            ctx.resolved_pool.as_ref(),
        ) {
            let cookie_key = extract_cookie_value(session, &sticky.config.cookie_name);
            let header_key = extract_header_value(session, &sticky.config.header_name);

            let affinity_key = if let Some(key) = cookie_key {
                key
            } else if let Some(key) = header_key {
                key
            } else {
                sticky.generate_affinity_key()
            };

            ctx.affinity_key = Some(affinity_key);
            // Always refresh cookie for sliding expiration.
            ctx.should_set_cookie = true;
        }

        // Deterministic behavior: Upgrade/WebSocket must go to an HTTP/1 pool.
        // If no HTTP/1 pool exists for the matched app, respond with a consistent error.
        if ctx.is_upgrade
            && let Some(app_name) = ctx.app_name.as_deref()
            && let Some(pools) = config.apps.get(app_name)
            && pools.ws.is_none()
        {
            ctx.failure_hint = Some(FailureHint::MissingWsPool);
            return Err(Error::explain(
                ErrorType::HTTPStatus(StatusCode::SERVICE_UNAVAILABLE.as_u16()),
                "no ws pool available for upgrade request",
            ));
        }

        if ctx.resolved_pool.is_none() {
            ctx.failure_hint = Some(FailureHint::MissingUpstreamPool);
            return Err(Error::explain(
                ErrorType::HTTPStatus(StatusCode::SERVICE_UNAVAILABLE.as_u16()),
                "no upstream pool resolved",
            ));
        }

        if let Some(resolved) = ctx.resolved_pool.as_ref() {
            ctx.selected_transport = Some(resolved.transport);
        }

        Ok(false)
    }

    async fn upstream_peer(
        &self,
        _session: &mut Session,
        ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        // Use pre-resolved pool from request_filter when available.
        let Some(resolved) = ctx.resolved_pool.as_ref() else {
            ctx.failure_hint = Some(FailureHint::MissingUpstreamPool);
            // Fallback: no pool was resolved (no app matched or no pools configured)
            return Err(Error::explain(
                ErrorType::HTTPStatus(StatusCode::SERVICE_UNAVAILABLE.as_u16()),
                "no upstream pool resolved",
            ));
        };

        let selected_addr = if let (Some(sticky), Some(app_name), Some(affinity_key)) = (
            self.0.sticky.as_ref(),
            ctx.app_name.as_deref(),
            ctx.affinity_key.as_deref(),
        ) {
            if let Some(mapped) = sticky.lookup(
                app_name,
                resolved.transport,
                affinity_key,
                &resolved.backends,
            ) {
                mapped
            } else {
                let Some(backend) = resolved.lb.select(affinity_key.as_bytes(), 8) else {
                    ctx.failure_hint = Some(FailureHint::UnhealthyUpstream);
                    return Err(Error::explain(
                        ErrorType::HTTPStatus(StatusCode::SERVICE_UNAVAILABLE.as_u16()),
                        "no healthy upstreams available",
                    ));
                };

                let selected = backend.addr.to_string();
                sticky.bind(app_name, resolved.transport, affinity_key, selected.clone());
                selected
            }
        } else {
            let Some(backend) = resolved.lb.select(b"", 8) else {
                ctx.failure_hint = Some(FailureHint::UnhealthyUpstream);
                return Err(Error::explain(
                    ErrorType::HTTPStatus(StatusCode::SERVICE_UNAVAILABLE.as_u16()),
                    "no healthy upstreams available",
                ));
            };
            backend.addr.to_string()
        };

        let mut peer = HttpPeer::new(
            selected_addr.as_str(),
            resolved.secure,
            resolved.verify_hostname.clone(),
        );
        // For plaintext HTTP/2 upstreams (h2c), Pingora needs the peer's min HTTP version to be 2,
        // otherwise it will assume HTTP/1.1 when no ALPN is present.
        if resolved.is_http2 {
            peer.options.set_http_version(2, 2);
        } else {
            peer.options.set_http_version(1, 1);
        }

        Ok(Box::new(peer))
    }

    async fn response_filter(
        &self,
        _session: &mut Session,
        upstream_response: &mut ResponseHeader,
        ctx: &mut Self::CTX,
    ) -> Result<()> {
        let (Some(sticky), Some(affinity_key)) =
            (self.0.sticky.as_ref(), ctx.affinity_key.as_deref())
        else {
            return Ok(());
        };

        if !ctx.should_set_cookie {
            return Ok(());
        }

        upstream_response
            .append_header(header::SET_COOKIE, sticky.cookie_header_value(affinity_key))
            .map_err(|e| {
                Error::because(
                    ErrorType::InternalError,
                    "failed to append sticky session cookie",
                    e,
                )
            })?;

        Ok(())
    }

    async fn fail_to_proxy(
        &self,
        session: &mut Session,
        e: &Error,
        ctx: &mut Self::CTX,
    ) -> FailToProxy {
        let code = match ctx.failure_hint {
            Some(FailureHint::MissingApplication) => 404,
            Some(FailureHint::MissingWsPool)
            | Some(FailureHint::MissingUpstreamPool)
            | Some(FailureHint::UnhealthyUpstream) => 503,
            None => match e.etype() {
                ErrorType::HTTPStatus(status) => *status,
                _ => 500,
            },
        };

        if code > 0 {
            session.respond_error(code).await.unwrap_or_else(|err| {
                log::error!("failed to send error response to downstream: {err}");
            });
        }

        FailToProxy {
            error_code: code,
            can_reuse_downstream: false,
        }
    }

    fn fail_to_connect(
        &self,
        _session: &mut Session,
        _peer: &HttpPeer,
        ctx: &mut Self::CTX,
        mut e: Box<Error>,
    ) -> Box<Error> {
        let (Some(sticky), Some(app_name), Some(transport), Some(affinity_key)) = (
            self.0.sticky.as_ref(),
            ctx.app_name.as_deref(),
            ctx.selected_transport,
            ctx.affinity_key.as_deref(),
        ) else {
            return e;
        };

        sticky.remove(app_name, transport, affinity_key);

        if !ctx.sticky_retry_attempted {
            ctx.sticky_retry_attempted = true;
            e.set_retry(true);
        }

        e
    }

    async fn upstream_request_filter(
        &self,
        _session: &mut Session,
        upstream_request: &mut RequestHeader,
        ctx: &mut Self::CTX,
    ) -> Result<()> {
        let Some(seg) = ctx.path_rewrite_segment.as_deref() else {
            return Ok(());
        };

        let Some(path_and_query) = upstream_request.uri.path_and_query().map(|pq| pq.as_str())
        else {
            return Ok(());
        };

        let Some(new_path_and_query) = strip_first_path_segment(path_and_query, seg) else {
            return Ok(());
        };

        let uri = Uri::builder()
            .path_and_query(new_path_and_query.as_ref())
            .build()
            .map_err(|e| {
                Error::because(
                    ErrorType::InternalError,
                    "failed to rewrite upstream uri",
                    e,
                )
            })?;

        upstream_request.set_uri(uri);
        Ok(())
    }
}

impl StickySessionState {
    fn lookup(
        &self,
        app_name: &str,
        transport: lb::TransportKind,
        affinity_key: &str,
        allowed_backends: &HashSet<String>,
    ) -> Option<String> {
        let now = Instant::now();
        let key = AffinityMapKey {
            app_name: app_name.to_string(),
            transport,
            affinity_key: affinity_key.to_string(),
        };

        let shard_index = self.shard_index_for_key(&key);
        let shard = &self.shards[shard_index];
        if self.should_cleanup(shard) {
            self.cleanup_global(now);
        }

        let mut entries = shard.entries.lock().ok()?;

        match entries.get_mut(&key) {
            Some(entry) if entry.expires_at > now && allowed_backends.contains(&entry.backend) => {
                entry.expires_at = now + self.config.ttl;
                self.record_eviction_candidate(shard_index, &key, entry.expires_at);
                Some(entry.backend.clone())
            }
            Some(_) => {
                entries.remove(&key);
                None
            }
            None => None,
        }
    }

    fn bind(
        &self,
        app_name: &str,
        transport: lb::TransportKind,
        affinity_key: &str,
        backend: String,
    ) {
        let key = AffinityMapKey {
            app_name: app_name.to_string(),
            transport,
            affinity_key: affinity_key.to_string(),
        };
        let now = Instant::now();
        let shard_index = self.shard_index_for_key(&key);
        let shard = &self.shards[shard_index];
        let expires_at = now + self.config.ttl;

        let Ok(mut entries) = shard.entries.lock() else {
            return;
        };

        entries.insert(
            key,
            AffinityEntry {
                backend,
                expires_at,
            },
        );

        self.record_eviction_candidate(
            shard_index,
            &AffinityMapKey {
                app_name: app_name.to_string(),
                transport,
                affinity_key: affinity_key.to_string(),
            },
            expires_at,
        );

        if !self.should_cleanup(shard) {
            return;
        }

        drop(entries);

        self.cleanup_global(now);
    }

    fn remove(&self, app_name: &str, transport: lb::TransportKind, affinity_key: &str) {
        let key = AffinityMapKey {
            app_name: app_name.to_string(),
            transport,
            affinity_key: affinity_key.to_string(),
        };
        let shard_index = self.shard_index_for_key(&key);
        let shard = &self.shards[shard_index];

        let Ok(mut entries) = shard.entries.lock() else {
            return;
        };
        entries.remove(&key);
    }

    fn generate_affinity_key(&self) -> String {
        let tick = self.key_counter.fetch_add(1, Ordering::Relaxed);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        format!("{:x}{:x}", now, tick)
    }

    fn cookie_header_value(&self, affinity_key: &str) -> String {
        let max_age_seconds = (self.config.ttl.as_secs().max(1)).to_string();

        if self.config.cookie_secure {
            format!(
                "{}={}; Max-Age={}; Path=/; HttpOnly; SameSite=Lax; Secure",
                self.config.cookie_name, affinity_key, max_age_seconds
            )
        } else {
            format!(
                "{}={}; Max-Age={}; Path=/; HttpOnly; SameSite=Lax",
                self.config.cookie_name, affinity_key, max_age_seconds
            )
        }
    }

    fn shard_index_for_key(&self, key: &AffinityMapKey) -> usize {
        let mut hasher = DefaultHasher::new();
        key.hash(&mut hasher);
        (hasher.finish() as usize) % self.shards.len()
    }

    fn record_eviction_candidate(
        &self,
        shard_index: usize,
        key: &AffinityMapKey,
        expires_at: Instant,
    ) {
        let Ok(mut heap) = self.eviction_index.lock() else {
            return;
        };

        heap.push(Reverse(EvictionItem {
            expires_at,
            sequence: self.eviction_counter.fetch_add(1, Ordering::Relaxed),
            shard_index,
            key: key.clone(),
        }));
    }

    fn should_cleanup(&self, shard: &StickyShard) -> bool {
        shard
            .op_counter
            .fetch_add(1, Ordering::Relaxed)
            .is_multiple_of(128)
    }

    fn cleanup_global(&self, now: Instant) {
        let mut shard_guards = self
            .shards
            .iter()
            .filter_map(|shard| shard.entries.lock().ok())
            .collect::<Vec<_>>();

        for entries in &mut shard_guards {
            entries.retain(|_, entry| entry.expires_at > now);
        }

        let current_entries = shard_guards
            .iter()
            .map(|entries| entries.len())
            .sum::<usize>();

        if current_entries <= self.config.max_entries {
            return;
        }

        let overflow = current_entries - self.config.max_entries;
        let mut removed = 0usize;

        if let Ok(mut heap) = self.eviction_index.lock() {
            while removed < overflow {
                let Some(Reverse(candidate)) = heap.pop() else {
                    break;
                };

                let Some(entries) = shard_guards.get_mut(candidate.shard_index) else {
                    continue;
                };

                let is_current = entries
                    .get(&candidate.key)
                    .map(|entry| entry.expires_at == candidate.expires_at)
                    .unwrap_or(false);

                if !is_current {
                    continue;
                }

                if entries.remove(&candidate.key).is_some() {
                    removed += 1;
                }
            }
        }

        if removed >= overflow {
            return;
        }

        let mut remaining = overflow - removed;
        for entries in &mut shard_guards {
            while remaining > 0 {
                let Some(key) = entries.keys().next().cloned() else {
                    break;
                };

                entries.remove(&key);
                remaining -= 1;
            }

            if remaining == 0 {
                break;
            }
        }
    }
}

fn extract_first_path_segment(session: &Session) -> Option<&str> {
    let path = session.req_header().uri.path();
    let mut parts = path.split('/').filter(|p| !p.is_empty());
    parts.next()
}

fn extract_host(session: &Session) -> Option<Cow<'_, str>> {
    let headers = &session.req_header().headers;

    let host_str = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .or_else(|| {
            headers
                .get(http::HeaderName::from_static(":authority"))
                .and_then(|v| v.to_str().ok())
        })?;

    let host_without_port = strip_port_str(host_str);

    // Only allocate if lowercase conversion is needed
    if host_without_port.chars().any(|c| c.is_ascii_uppercase()) {
        Some(Cow::Owned(host_without_port.to_ascii_lowercase()))
    } else {
        Some(Cow::Borrowed(host_without_port))
    }
}

fn extract_cookie_value(session: &Session, cookie_name: &str) -> Option<String> {
    let raw = session
        .req_header()
        .headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())?;

    raw.split(';').find_map(|pair| {
        let (name, value) = pair.trim().split_once('=')?;
        if name == cookie_name {
            let value = value.trim();
            if value.is_empty() || value.len() > MAX_AFFINITY_KEY_LEN {
                None
            } else {
                Some(value.to_string())
            }
        } else {
            None
        }
    })
}

fn extract_header_value(session: &Session, header_name: &str) -> Option<String> {
    session
        .req_header()
        .headers
        .get(header_name)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty() && v.len() <= MAX_AFFINITY_KEY_LEN)
}

fn strip_first_path_segment<'a>(path_and_query: &'a str, segment: &str) -> Option<Cow<'a, str>> {
    let (path, query) = match path_and_query.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (path_and_query, None),
    };

    let prefix_len = segment.len() + 1; // "/{segment}".len()

    // Check if path matches "/{segment}" exactly or starts with "/{segment}/"
    if !path.starts_with('/') || path.len() < prefix_len {
        return None;
    }

    let after_slash = &path[1..];
    if !after_slash.starts_with(segment) {
        return None;
    }

    // Check boundary: must be exact match or followed by '/'
    let remainder = &path[prefix_len..];
    let rewritten_path = if remainder.is_empty() {
        // path == "/{segment}"
        "/"
    } else if remainder.starts_with('/') {
        // path starts with "/{segment}/"
        remainder
    } else {
        // path is like "/{segment}xyz" - not a boundary match
        return None;
    };

    // If no query string, we can return a borrowed slice
    match query {
        None => Some(Cow::Borrowed(rewritten_path)),
        Some(q) => {
            // Must allocate to concatenate path + "?" + query
            let mut out = String::with_capacity(rewritten_path.len() + 1 + q.len());
            out.push_str(rewritten_path);
            out.push('?');
            out.push_str(q);
            Some(Cow::Owned(out))
        }
    }
}

/// Strip port from host string, returning a slice (zero allocation).
fn strip_port_str(host: &str) -> &str {
    // "example.com:3000" => "example.com"
    // "[::1]:3000" => "[::1]"
    if let Some(stripped) = host.strip_prefix('[') {
        // IPv6 address: find closing bracket
        if let Some(end) = stripped.find(']') {
            return &host[..end + 2]; // Include brackets: "[" + content + "]"
        }
        return host;
    }

    match host.rsplit_once(':') {
        Some((h, port)) if !h.is_empty() && port.chars().all(|c| c.is_ascii_digit()) => h,
        _ => host,
    }
}

#[cfg(test)]
mod tests {
    use super::strip_first_path_segment;

    #[test]
    fn path_rewrite_strips_exact_segment() {
        assert_eq!(
            strip_first_path_segment("/auth", "auth").as_deref(),
            Some("/")
        );
        assert_eq!(
            strip_first_path_segment("/auth/", "auth").as_deref(),
            Some("/")
        );
        assert_eq!(
            strip_first_path_segment("/auth/login", "auth").as_deref(),
            Some("/login")
        );
    }

    #[test]
    fn path_rewrite_preserves_query_string() {
        assert_eq!(
            strip_first_path_segment("/auth/login?x=1", "auth").as_deref(),
            Some("/login?x=1")
        );
    }

    #[test]
    fn path_rewrite_is_boundary_aware() {
        assert_eq!(strip_first_path_segment("/authz", "auth"), None);
        assert_eq!(strip_first_path_segment("/authz/login", "auth"), None);
        assert_eq!(strip_first_path_segment("/a", "auth"), None);
    }

    #[test]
    fn strip_port_handles_ipv4() {
        use super::strip_port_str;
        assert_eq!(strip_port_str("example.com:3000"), "example.com");
        assert_eq!(strip_port_str("example.com"), "example.com");
        assert_eq!(strip_port_str("127.0.0.1:8080"), "127.0.0.1");
    }

    #[test]
    fn strip_port_handles_ipv6() {
        use super::strip_port_str;
        assert_eq!(strip_port_str("[::1]:3000"), "[::1]");
        assert_eq!(strip_port_str("[::1]"), "[::1]");
        assert_eq!(strip_port_str("[2001:db8::1]:443"), "[2001:db8::1]");
    }

    #[test]
    fn strip_port_edge_cases() {
        use super::strip_port_str;
        // Empty string
        assert_eq!(strip_port_str(""), "");
        // Trailing colon - empty "port" is all digits (vacuously true), so strips
        assert_eq!(strip_port_str("host:"), "host");
        // Non-numeric port (should not strip)
        assert_eq!(strip_port_str("host:abc"), "host:abc");
        // Multiple colons without brackets (last segment is port-like)
        assert_eq!(strip_port_str("a:b:80"), "a:b");
        // Only port number - empty host, does not strip
        assert_eq!(strip_port_str(":8080"), ":8080");
        // Malformed IPv6 (no closing bracket)
        assert_eq!(strip_port_str("[::1"), "[::1");
        // IPv6 with trailing content after bracket
        assert_eq!(strip_port_str("[::1]abc"), "[::1]");
    }

    #[test]
    fn path_rewrite_edge_cases() {
        // Root path - no segment to strip
        assert_eq!(strip_first_path_segment("/", "auth"), None);
        // Empty segment name
        assert_eq!(strip_first_path_segment("/auth", ""), None);
        // Deeply nested paths
        assert_eq!(
            strip_first_path_segment("/auth/a/b/c/d", "auth").as_deref(),
            Some("/a/b/c/d")
        );
        // Query string only on root segment
        assert_eq!(
            strip_first_path_segment("/auth?redirect=home", "auth").as_deref(),
            Some("/?redirect=home")
        );
        // Multiple query parameters
        assert_eq!(
            strip_first_path_segment("/auth/login?a=1&b=2&c=3", "auth").as_deref(),
            Some("/login?a=1&b=2&c=3")
        );
        // Path with encoded characters
        assert_eq!(
            strip_first_path_segment("/auth/path%20with%20spaces", "auth").as_deref(),
            Some("/path%20with%20spaces")
        );
        // Segment with special chars (if segment itself has special chars)
        assert_eq!(
            strip_first_path_segment("/auth-service/login", "auth-service").as_deref(),
            Some("/login")
        );
    }

    #[test]
    fn path_rewrite_returns_borrowed_when_no_query() {
        use std::borrow::Cow;
        // Without query string, should return Cow::Borrowed
        let result = strip_first_path_segment("/auth/login", "auth");
        assert!(matches!(result, Some(Cow::Borrowed(_))));

        // With query string, must allocate (Cow::Owned)
        let result = strip_first_path_segment("/auth/login?x=1", "auth");
        assert!(matches!(result, Some(Cow::Owned(_))));
    }

    #[test]
    fn path_rewrite_no_match_cases() {
        // Completely different segment
        assert_eq!(strip_first_path_segment("/users/login", "auth"), None);
        // Segment is prefix but not at boundary
        assert_eq!(strip_first_path_segment("/authorization", "auth"), None);
        // Case sensitive - should not match
        assert_eq!(strip_first_path_segment("/Auth/login", "auth"), None);
        assert_eq!(strip_first_path_segment("/AUTH/login", "auth"), None);
        // Missing leading slash
        assert_eq!(strip_first_path_segment("auth/login", "auth"), None);
    }

    #[test]
    fn lowercase_host_helper() {
        use super::strip_port_str;
        use std::borrow::Cow;

        // Helper to test the lowercase Cow logic (extracted from extract_host)
        fn normalize_host(host: &str) -> Cow<'_, str> {
            let host_without_port = strip_port_str(host);
            if host_without_port.chars().any(|c| c.is_ascii_uppercase()) {
                Cow::Owned(host_without_port.to_ascii_lowercase())
            } else {
                Cow::Borrowed(host_without_port)
            }
        }

        // Already lowercase - should borrow
        let result = normalize_host("example.com");
        assert!(matches!(result, Cow::Borrowed(_)));
        assert_eq!(result, "example.com");

        // Uppercase - should allocate and lowercase
        let result = normalize_host("Example.COM");
        assert!(matches!(result, Cow::Owned(_)));
        assert_eq!(result, "example.com");

        // Mixed case with port
        let result = normalize_host("Example.com:8080");
        assert!(matches!(result, Cow::Owned(_)));
        assert_eq!(result, "example.com");

        // Lowercase with port - should borrow
        let result = normalize_host("example.com:8080");
        assert!(matches!(result, Cow::Borrowed(_)));
        assert_eq!(result, "example.com");

        // IPv6 uppercase (rare but possible)
        let result = normalize_host("[::1]:8080");
        assert!(matches!(result, Cow::Borrowed(_)));
        assert_eq!(result, "[::1]");
    }
}
