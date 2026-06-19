use crate::errors;
use napi::bindgen_prelude::*;
use napi_derive::napi;
use std::collections::HashSet;

pub mod defaults {
    pub const MAX_URI_BYTES: usize = 8_192;
    pub const MAX_REQUEST_HEADERS: usize = 100;
    pub const MAX_SINGLE_HEADER_BYTES: usize = 8_192;
    pub const MAX_REQUEST_HEADER_BYTES: usize = 64 * 1024;
    pub const MAX_REQUEST_BODY_BYTES: u64 = 16 * 1024 * 1024;

    pub const DOWNSTREAM_HEADER_TIMEOUT_MS: u32 = 10_000;
    pub const DOWNSTREAM_BODY_IDLE_TIMEOUT_MS: u32 = 60_000;
    pub const UPSTREAM_CONNECT_TIMEOUT_MS: u32 = 5_000;
    pub const UPSTREAM_READ_TIMEOUT_MS: u32 = 60_000;
    pub const UPSTREAM_WRITE_TIMEOUT_MS: u32 = 60_000;
    pub const DOWNSTREAM_KEEP_ALIVE_TIMEOUT_SECONDS: u32 = 75;
}

#[napi(object)]
pub struct ProxyOptions {
    pub listen: String,
    pub tls: Option<ListenerTlsOptions>,
    pub applications: Vec<ApplicationOptions>,
    /// Health check interval in milliseconds. Defaults to 5000ms (5 seconds).
    pub health_check_interval_ms: Option<u32>,
    /// Sticky session configuration.
    pub sticky_sessions: Option<StickySessionOptions>,
    /// Request size and header limits. All size values are bytes. Set null to disable all Neemata request limit checks.
    pub limits: Option<Either<ProxyLimitsOptions, Null>>,
    /// Downstream and upstream timeout configuration. Values are milliseconds except downstreamKeepAlive, which is seconds.
    pub timeouts: Option<ProxyTimeoutOptions>,
}

#[napi(object)]
pub struct ProxyLimitsOptions {
    /// Maximum request URI size, in bytes (default: 8192 bytes / 8 KiB). Set null to disable this check.
    pub max_uri_size: Option<Either<u32, Null>>,
    /// Maximum request header count (default: 100). Set null to disable this check.
    pub max_request_headers: Option<Either<u32, Null>>,
    /// Maximum single request header size, in bytes (default: 8192 bytes / 8 KiB). Set null to disable this check.
    pub max_single_header_size: Option<Either<u32, Null>>,
    /// Maximum total request header size, in bytes (default: 65536 bytes / 64 KiB). Set null to disable this check.
    pub max_request_header_size: Option<Either<u32, Null>>,
    /// Maximum Content-Length accepted, in bytes (default: 16777216 bytes / 16 MiB). Set null to disable this check. Chunked or unknown-length body enforcement depends on future Pingora early body buffering support.
    pub max_request_body_size: Option<Either<u32, Null>>,
}

#[napi(object)]
pub struct ProxyTimeoutOptions {
    /// Timeout for receiving downstream request headers, in milliseconds (default: 10000 ms / 10s). Parsed and validated, but not enforced until Pingora exposes a pre-header-read hook.
    pub downstream_header: Option<u32>,
    /// Idle timeout between downstream HTTP/1 request body chunks, in milliseconds (default: 60000 ms / 60s / 1min). Pingora currently no-ops this for downstream HTTP/2.
    pub downstream_body_idle: Option<u32>,
    /// Timeout for connecting to upstream, in milliseconds (default: 5000 ms / 5s).
    pub upstream_connect: Option<u32>,
    /// Idle timeout while reading upstream response data, in milliseconds (default: 60000 ms / 60s / 1min).
    pub upstream_read: Option<u32>,
    /// Idle timeout while writing request data upstream, in milliseconds (default: 60000 ms / 60s / 1min).
    pub upstream_write: Option<u32>,
    /// Downstream HTTP/1 keep-alive idle timeout, in seconds (default: 75 seconds / 1m15s). Pingora currently no-ops this for downstream HTTP/2.
    pub downstream_keep_alive: Option<u32>,
}

#[napi(object)]
pub struct StickySessionOptions {
    /// Whether sticky sessions are enabled.
    pub enabled: Option<bool>,
    /// Cookie name used to persist affinity key (default: "nmt_affinity").
    pub cookie_name: Option<String>,
    /// Header name used for custom affinity key (default: "x-nmt-affinity-key").
    pub header_name: Option<String>,
    /// Sliding expiration TTL in milliseconds (default: 600000 = 10 minutes).
    pub ttl_ms: Option<u32>,
    /// Maximum affinity entries kept in memory (default: 20000).
    pub max_entries: Option<u32>,
}

#[napi(object)]
pub struct ListenerTlsOptions {
    pub cert_path: String,
    pub key_path: String,
    pub enable_h2: Option<bool>,
}

#[napi(object)]
pub struct ApplicationOptions {
    pub name: String,
    pub routing: ProxyApplicationRouting,
    pub sni: Option<String>,
}

#[napi(object_from_js, discriminant = "type", discriminant_case = "lowercase")]
pub enum ProxyApplicationRouting {
    Path { name: Option<String> },
    Subdomain { name: Option<String> },
    Default,
}

#[derive(Debug, Clone)]
pub struct ProxyOptionsParsed {
    pub listen: String,
    pub tls: Option<ListenerTlsOptionsParsed>,
    pub applications: Vec<ApplicationOptionsParsed>,
    /// Health check interval in milliseconds.
    pub health_check_interval_ms: u32,
    pub sticky_sessions: StickySessionOptionsParsed,
    pub limits: ProxyLimitsOptionsParsed,
    pub timeouts: ProxyTimeoutOptionsParsed,
}

#[derive(Debug, Clone)]
pub struct StickySessionOptionsParsed {
    pub enabled: bool,
    pub cookie_name: String,
    pub header_name: String,
    pub ttl_ms: u32,
    pub max_entries: usize,
}

#[derive(Debug, Clone)]
pub enum ProxyLimitsOptionsParsed {
    Disabled,
    Enabled { checks: Vec<ProxyLimitCheckParsed> },
}

#[derive(Debug, Clone)]
pub enum ProxyLimitCheckParsed {
    UriBytes(usize),
    RequestHeaders(usize),
    SingleHeaderBytes(usize),
    RequestHeaderBytes(usize),
    RequestBodyBytes(u64),
}

#[derive(Debug, Clone)]
pub struct ProxyTimeoutOptionsParsed {
    #[allow(dead_code)]
    pub downstream_header_timeout_ms: u32,
    pub downstream_body_idle_timeout_ms: u32,
    pub upstream_connect_timeout_ms: u32,
    pub upstream_read_timeout_ms: u32,
    pub upstream_write_timeout_ms: u32,
    pub downstream_keep_alive_timeout_seconds: u32,
}

#[derive(Debug, Clone)]
pub struct ListenerTlsOptionsParsed {
    pub cert_path: String,
    pub key_path: String,
    pub enable_h2: bool,
}

#[derive(Debug, Clone)]
pub struct ApplicationOptionsParsed {
    pub name: String,
    pub routing: ApplicationRoutingParsed,
    pub sni: Option<String>,
}

#[derive(Debug, Clone)]
pub enum ApplicationRoutingParsed {
    Subdomain { name: String },
    Path { name: String },
    Default,
}

pub fn parse_proxy_options(env: &Env, options: ProxyOptions) -> Result<ProxyOptionsParsed> {
    let listen = options.listen;

    let tls = options.tls.map(|tls| ListenerTlsOptionsParsed {
        cert_path: tls.cert_path,
        key_path: tls.key_path,
        enable_h2: tls.enable_h2.unwrap_or(false),
    });

    let mut applications = Vec::with_capacity(options.applications.len());
    for app in options.applications {
        applications.push(parse_application_options(env, app)?);
    }

    validate_applications(env, &applications)?;

    let sticky_sessions = parse_sticky_session_options(env, options.sticky_sessions)?;
    let limits = parse_limits_options(env, options.limits)?;
    let timeouts = parse_timeout_options(env, options.timeouts)?;

    Ok(ProxyOptionsParsed {
        listen,
        tls,
        applications,
        health_check_interval_ms: options.health_check_interval_ms.unwrap_or(5000),
        sticky_sessions,
        limits,
        timeouts,
    })
}

fn parse_limits_options(
    env: &Env,
    limits: Option<Either<ProxyLimitsOptions, Null>>,
) -> Result<ProxyLimitsOptionsParsed> {
    let Some(limits) = limits else {
        return Ok(default_limit_checks());
    };

    let Either::A(limits) = limits else {
        return Ok(ProxyLimitsOptionsParsed::Disabled);
    };

    let mut checks = Vec::with_capacity(5);

    if let Some(limit) = parse_nullable_positive_usize(
        env,
        "limits.maxUriSize",
        limits.max_uri_size,
        defaults::MAX_URI_BYTES,
    )? {
        checks.push(ProxyLimitCheckParsed::UriBytes(limit));
    }

    if let Some(limit) = parse_nullable_positive_usize(
        env,
        "limits.maxRequestHeaders",
        limits.max_request_headers,
        defaults::MAX_REQUEST_HEADERS,
    )? {
        checks.push(ProxyLimitCheckParsed::RequestHeaders(limit));
    }

    if let Some(limit) = parse_nullable_positive_usize(
        env,
        "limits.maxSingleHeaderSize",
        limits.max_single_header_size,
        defaults::MAX_SINGLE_HEADER_BYTES,
    )? {
        checks.push(ProxyLimitCheckParsed::SingleHeaderBytes(limit));
    }

    if let Some(limit) = parse_nullable_positive_usize(
        env,
        "limits.maxRequestHeaderSize",
        limits.max_request_header_size,
        defaults::MAX_REQUEST_HEADER_BYTES,
    )? {
        checks.push(ProxyLimitCheckParsed::RequestHeaderBytes(limit));
    }

    if let Some(limit) = parse_nullable_positive_u64(
        env,
        "limits.maxRequestBodySize",
        limits.max_request_body_size,
        defaults::MAX_REQUEST_BODY_BYTES,
    )? {
        checks.push(ProxyLimitCheckParsed::RequestBodyBytes(limit));
    }

    Ok(if checks.is_empty() {
        ProxyLimitsOptionsParsed::Disabled
    } else {
        ProxyLimitsOptionsParsed::Enabled { checks }
    })
}

fn default_limit_checks() -> ProxyLimitsOptionsParsed {
    ProxyLimitsOptionsParsed::Enabled {
        checks: vec![
            ProxyLimitCheckParsed::UriBytes(defaults::MAX_URI_BYTES),
            ProxyLimitCheckParsed::RequestHeaders(defaults::MAX_REQUEST_HEADERS),
            ProxyLimitCheckParsed::SingleHeaderBytes(defaults::MAX_SINGLE_HEADER_BYTES),
            ProxyLimitCheckParsed::RequestHeaderBytes(defaults::MAX_REQUEST_HEADER_BYTES),
            ProxyLimitCheckParsed::RequestBodyBytes(defaults::MAX_REQUEST_BODY_BYTES),
        ],
    }
}

fn parse_timeout_options(
    env: &Env,
    timeouts: Option<ProxyTimeoutOptions>,
) -> Result<ProxyTimeoutOptionsParsed> {
    let Some(timeouts) = timeouts else {
        return Ok(ProxyTimeoutOptionsParsed {
            downstream_header_timeout_ms: defaults::DOWNSTREAM_HEADER_TIMEOUT_MS,
            downstream_body_idle_timeout_ms: defaults::DOWNSTREAM_BODY_IDLE_TIMEOUT_MS,
            upstream_connect_timeout_ms: defaults::UPSTREAM_CONNECT_TIMEOUT_MS,
            upstream_read_timeout_ms: defaults::UPSTREAM_READ_TIMEOUT_MS,
            upstream_write_timeout_ms: defaults::UPSTREAM_WRITE_TIMEOUT_MS,
            downstream_keep_alive_timeout_seconds: defaults::DOWNSTREAM_KEEP_ALIVE_TIMEOUT_SECONDS,
        });
    };

    Ok(ProxyTimeoutOptionsParsed {
        downstream_header_timeout_ms: parse_positive_u32(
            env,
            "timeouts.downstreamHeader",
            timeouts.downstream_header,
            defaults::DOWNSTREAM_HEADER_TIMEOUT_MS,
        )?,
        downstream_body_idle_timeout_ms: parse_positive_u32(
            env,
            "timeouts.downstreamBodyIdle",
            timeouts.downstream_body_idle,
            defaults::DOWNSTREAM_BODY_IDLE_TIMEOUT_MS,
        )?,
        upstream_connect_timeout_ms: parse_positive_u32(
            env,
            "timeouts.upstreamConnect",
            timeouts.upstream_connect,
            defaults::UPSTREAM_CONNECT_TIMEOUT_MS,
        )?,
        upstream_read_timeout_ms: parse_positive_u32(
            env,
            "timeouts.upstreamRead",
            timeouts.upstream_read,
            defaults::UPSTREAM_READ_TIMEOUT_MS,
        )?,
        upstream_write_timeout_ms: parse_positive_u32(
            env,
            "timeouts.upstreamWrite",
            timeouts.upstream_write,
            defaults::UPSTREAM_WRITE_TIMEOUT_MS,
        )?,
        downstream_keep_alive_timeout_seconds: parse_positive_u32(
            env,
            "timeouts.downstreamKeepAlive",
            timeouts.downstream_keep_alive,
            defaults::DOWNSTREAM_KEEP_ALIVE_TIMEOUT_SECONDS,
        )?,
    })
}

fn parse_nullable_positive_usize(
    env: &Env,
    field: &str,
    value: Option<Either<u32, Null>>,
    default: usize,
) -> Result<Option<usize>> {
    Ok(parse_nullable_positive_u32(env, field, value, default as u32)?.map(|value| value as usize))
}

fn parse_nullable_positive_u64(
    env: &Env,
    field: &str,
    value: Option<Either<u32, Null>>,
    default: u64,
) -> Result<Option<u64>> {
    Ok(parse_nullable_positive_u32(env, field, value, default as u32)?.map(|value| value as u64))
}

fn parse_nullable_positive_u32(
    env: &Env,
    field: &str,
    value: Option<Either<u32, Null>>,
    default: u32,
) -> Result<Option<u32>> {
    let value = match value {
        Some(Either::A(value)) => value,
        Some(Either::B(_)) => return Ok(None),
        None => default,
    };

    if value == 0 {
        return errors::throw_type_error(
            env,
            errors::codes::INVALID_PROXY_OPTIONS,
            format!("{field} must be greater than 0"),
        );
    }

    Ok(Some(value))
}

fn parse_positive_u32(env: &Env, field: &str, value: Option<u32>, default: u32) -> Result<u32> {
    let value = value.unwrap_or(default);
    if value == 0 {
        return errors::throw_type_error(
            env,
            errors::codes::INVALID_PROXY_OPTIONS,
            format!("{field} must be greater than 0"),
        );
    }
    Ok(value)
}

fn parse_sticky_session_options(
    env: &Env,
    sticky: Option<StickySessionOptions>,
) -> Result<StickySessionOptionsParsed> {
    let Some(sticky) = sticky else {
        return Ok(StickySessionOptionsParsed {
            enabled: false,
            cookie_name: "nmt_affinity".to_string(),
            header_name: "x-nmt-affinity-key".to_string(),
            ttl_ms: 600_000,
            max_entries: 20_000,
        });
    };

    let enabled = sticky.enabled.unwrap_or(true);
    let cookie_name = sticky
        .cookie_name
        .unwrap_or_else(|| "nmt_affinity".to_string());
    let header_name = sticky
        .header_name
        .unwrap_or_else(|| "x-nmt-affinity-key".to_string());
    let ttl_ms = sticky.ttl_ms.unwrap_or(600_000);
    let max_entries = sticky.max_entries.unwrap_or(20_000);

    if cookie_name.trim().is_empty() {
        return errors::throw_type_error(
            env,
            errors::codes::INVALID_PROXY_OPTIONS,
            "stickySessions.cookieName must be non-empty",
        );
    }

    if header_name.trim().is_empty() {
        return errors::throw_type_error(
            env,
            errors::codes::INVALID_PROXY_OPTIONS,
            "stickySessions.headerName must be non-empty",
        );
    }

    if ttl_ms == 0 {
        return errors::throw_type_error(
            env,
            errors::codes::INVALID_PROXY_OPTIONS,
            "stickySessions.ttlMs must be greater than 0",
        );
    }

    if max_entries == 0 {
        return errors::throw_type_error(
            env,
            errors::codes::INVALID_PROXY_OPTIONS,
            "stickySessions.maxEntries must be greater than 0",
        );
    }

    Ok(StickySessionOptionsParsed {
        enabled,
        cookie_name,
        header_name,
        ttl_ms,
        max_entries: max_entries as usize,
    })
}

fn parse_application_options(
    env: &Env,
    app: ApplicationOptions,
) -> Result<ApplicationOptionsParsed> {
    let name = app.name;
    let sni = parse_application_sni(env, app.sni)?;
    let routing = parse_routing(env, app.routing)?;
    Ok(ApplicationOptionsParsed { name, routing, sni })
}

fn parse_application_sni(env: &Env, sni: Option<String>) -> Result<Option<String>> {
    let Some(sni) = sni else {
        return Ok(None);
    };

    let sni = sni.trim();
    if sni.is_empty() {
        return errors::throw_type_error(
            env,
            errors::codes::INVALID_APPLICATION_OPTIONS,
            "ApplicationOptions.sni must be non-empty when provided",
        );
    }

    if !is_hostname_like(sni) {
        return errors::throw_type_error(
            env,
            errors::codes::INVALID_APPLICATION_OPTIONS,
            "ApplicationOptions.sni must be a hostname-like value",
        );
    }

    Ok(Some(sni.to_string()))
}

fn is_hostname_like(value: &str) -> bool {
    if value.starts_with('.') || value.ends_with('.') {
        return false;
    }

    value.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
    })
}

fn parse_routing(env: &Env, routing: ProxyApplicationRouting) -> Result<ApplicationRoutingParsed> {
    match routing {
        ProxyApplicationRouting::Default => Ok(ApplicationRoutingParsed::Default),
        ProxyApplicationRouting::Subdomain { name } => {
            let name = require_routing_name(env, "subdomain", name)?;
            Ok(ApplicationRoutingParsed::Subdomain { name })
        }
        ProxyApplicationRouting::Path { name } => {
            let name = require_routing_name(env, "path", name)?;
            Ok(ApplicationRoutingParsed::Path { name })
        }
    }
}

fn require_routing_name(env: &Env, routing_type: &str, name: Option<String>) -> Result<String> {
    let Some(name) = name else {
        return errors::throw_type_error(
            env,
            errors::codes::INVALID_APPLICATION_OPTIONS,
            format!("ApplicationOptions.routing.name must be set for {routing_type} routing"),
        );
    };

    Ok(name)
}

fn validate_applications(env: &Env, applications: &[ApplicationOptionsParsed]) -> Result<()> {
    let mut default_count = 0usize;
    let mut app_names = HashSet::new();
    let mut subdomain_routes = HashSet::new();
    let mut path_routes = HashSet::new();

    for app in applications {
        if !app_names.insert(app.name.clone()) {
            return errors::throw_type_error(
                env,
                errors::codes::INVALID_APPLICATION_OPTIONS,
                format!("Duplicate application name '{}'", app.name),
            );
        }

        match &app.routing {
            ApplicationRoutingParsed::Default => {
                default_count += 1;
            }
            ApplicationRoutingParsed::Subdomain { name } => {
                if name.is_empty() {
                    return errors::throw_type_error(
                        env,
                        errors::codes::INVALID_APPLICATION_OPTIONS,
                        "subdomain routing name must be non-empty",
                    );
                }

                let route_key = name.to_ascii_lowercase();
                if !subdomain_routes.insert(route_key) {
                    return errors::throw_type_error(
                        env,
                        errors::codes::INVALID_APPLICATION_OPTIONS,
                        format!("Duplicate subdomain routing name '{}'", name),
                    );
                }
            }
            ApplicationRoutingParsed::Path { name } => {
                if name.is_empty() {
                    return errors::throw_type_error(
                        env,
                        errors::codes::INVALID_APPLICATION_OPTIONS,
                        "path routing name must be non-empty",
                    );
                }
                if name.contains('/') {
                    return errors::throw_type_error(
                        env,
                        errors::codes::INVALID_APPLICATION_OPTIONS,
                        "path routing name must not contain '/'",
                    );
                }

                if !path_routes.insert(name.clone()) {
                    return errors::throw_type_error(
                        env,
                        errors::codes::INVALID_APPLICATION_OPTIONS,
                        format!("Duplicate path routing name '{}'", name),
                    );
                }
            }
        }
    }

    if default_count > 1 {
        return errors::throw_type_error(
            env,
            errors::codes::INVALID_APPLICATION_OPTIONS,
            "At most one application may use routing.type='default'",
        );
    }

    Ok(())
}
