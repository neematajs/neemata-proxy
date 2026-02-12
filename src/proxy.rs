use crate::{errors, lb, options, router, server};
use napi::bindgen_prelude::*;
use napi_derive::napi;
use pingora::lb::{LoadBalancer, health_check::TcpHealthCheck, selection::RoundRobin};
use std::collections::{HashMap, HashSet};
use std::net::ToSocketAddrs;
#[cfg(unix)]
use std::os::unix::io::IntoRawFd;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{Mutex as TokioMutex, watch};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
enum ProxyState {
    Stopped,
    Starting,
    Running,
    Stopping,
}

struct ProxyInner {
    options: options::ProxyOptionsParsed,
    state: ProxyState,
    server: server::Server,
    router: Arc<router::Router>,
    // Upstreams are mutable; app definitions are immutable (from `options`).
    upstreams_by_app: HashMap<String, HashSet<PortUpstream>>,
    // One pool per (app, transport).
    pools: HashMap<(String, lb::TransportKind), router::PoolConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct PortUpstream {
    transport: lb::TransportKind,
    secure: bool,
    hostname: String,
    port: u16,
}

#[napi(object)]
pub struct PortUpstreamOptions {
    #[napi(ts_type = " 'port' ")]
    pub r#type: String,
    #[napi(ts_type = " 'http' | 'http2' | 'ws' ")]
    pub transport: String,
    pub secure: bool,
    pub hostname: String,
    pub port: u32,
}

#[napi(object)]
pub struct UnixSocketUpstreamOptions {
    #[napi(ts_type = " 'unix' ")]
    pub r#type: String,
    #[napi(ts_type = " 'http' | 'http2' | 'ws' ")]
    pub transport: String,
    pub secure: bool,
    pub path: String,
}

#[napi]
pub type UpstreamOptions = Either<PortUpstreamOptions, UnixSocketUpstreamOptions>;

#[napi]
pub struct Proxy {
    inner: Arc<Mutex<ProxyInner>>,
}

#[napi]
impl Proxy {
    #[napi(constructor)]
    pub fn new(env: Env, options: options::ProxyOptions) -> Result<Self> {
        let parsed = options::parse_proxy_options(&env, options)?;

        let router = Arc::new(router::Router::new(
            router::RouterConfig::default(),
            router::StickySessionConfig {
                enabled: parsed.sticky_sessions.enabled,
                cookie_name: parsed.sticky_sessions.cookie_name.clone(),
                header_name: parsed.sticky_sessions.header_name.clone(),
                ttl: Duration::from_millis(parsed.sticky_sessions.ttl_ms as u64),
                max_entries: parsed.sticky_sessions.max_entries,
                cookie_secure: parsed.tls.is_some(),
            },
        ));

        let mut upstreams_by_app = HashMap::new();
        for app in &parsed.applications {
            upstreams_by_app.insert(app.name.clone(), HashSet::new());
        }

        let inner = ProxyInner {
            options: parsed,
            state: ProxyState::Stopped,
            server: server::Server::new(),
            router,
            upstreams_by_app,
            pools: HashMap::new(),
        };

        // Initial router config: routes known, pools empty.
        let initial_config = build_router_config(&inner.options, &inner.pools);
        inner.router.update(initial_config);

        Ok(Self {
            inner: Arc::new(Mutex::new(inner)),
        })
    }

    #[napi]
    pub fn start<'env>(&self, env: &'env Env) -> Result<PromiseRaw<'env, ()>> {
        // Requirement: `start()` resolves only after the bind attempt completes.
        // We do the bind synchronously here so we can throw a JS Error with a stable `code`.
        let mut guard = self
            .inner
            .lock()
            .map_err(|_| napi::Error::from_reason("Proxy mutex poisoned"))?;

        match guard.state {
            ProxyState::Stopped => {
                guard.state = ProxyState::Starting;
            }
            ProxyState::Starting => {
                // Block until the other start() finishes binding by waiting for the mutex.
                // By the time we get here, we already hold the mutex, so just decide based on
                // the current state.
                return env.spawn_future(async { Ok(()) });
            }
            ProxyState::Running => {
                return Err(errors::generic_error(
                    env,
                    errors::codes::ALREADY_STARTED,
                    "Proxy is already started",
                ));
            }
            ProxyState::Stopping => {
                return env.spawn_future(async { Ok(()) });
            }
        }

        let listen = guard.options.listen.clone();
        let tls = guard.options.tls.clone();
        let server = guard.server.clone();
        let shared_router = router::SharedRouter::new(guard.router.clone());
        let initial_lb_tasks = guard
            .pools
            .iter()
            .map(|((app, transport), pool)| (lb_service_name(app, *transport), pool.lb.clone()))
            .collect::<Vec<_>>();

        let std_listener = std::net::TcpListener::bind(&listen).map_err(|e| {
            // Transition back to Stopped on bind failure.
            guard.state = ProxyState::Stopped;
            errors::generic_error(
                env,
                errors::codes::LISTEN_BIND_FAILED,
                format!("Failed to bind listener on '{listen}': {e}"),
            )
        })?;
        std_listener.set_nonblocking(true).map_err(|e| {
            napi::Error::from_reason(format!("Failed to set listener nonblocking: {e}"))
        })?;

        let tls_settings = if let Some(tls) = tls {
            let mut settings =
                pingora::listeners::tls::TlsSettings::intermediate(&tls.cert_path, &tls.key_path)
                    .map_err(|e| {
                    // Transition back to Stopped on TLS config failure.
                    guard.state = ProxyState::Stopped;
                    errors::generic_error(
                        env,
                        errors::codes::INVALID_PROXY_OPTIONS,
                        format!(
                            "Failed to configure TLS using cert '{}' and key '{}': {e}",
                            tls.cert_path, tls.key_path
                        ),
                    )
                })?;
            if tls.enable_h2 {
                settings.enable_h2();
            }
            Some(settings)
        } else {
            None
        };

        // At this point, bind + TLS config are done.
        // Mark Running before releasing the mutex so concurrent start/stop behave deterministically.
        guard.state = ProxyState::Running;
        drop(guard);

        // Move listener into tokio, start accept loop, then resolve.
        env.spawn_future(async move {
            let cancel = CancellationToken::new();
            let cancel_child = cancel.clone();

            // Start LB health tasks first so pools become ready.
            for (svc_name, lb) in initial_lb_tasks {
                server
                    .upsert_service(svc_name, lb::spawn_lb_health_task(lb))
                    .await;
            }

            // Hand the already-bound socket over to Pingora via the `ListenFds` table.
            // This ensures the bind attempt (and any bind errors) happen before Pingora
            // starts, avoiding its internal `expect("Failed to build listeners")` panic.
            #[cfg(unix)]
            let fd = std_listener.into_raw_fd();

            #[cfg(unix)]
            let mut fds = pingora::server::Fds::new();
            #[cfg(unix)]
            fds.add(listen.clone(), fd);

            #[cfg(unix)]
            let listen_fds: pingora::server::ListenFds = Arc::new(TokioMutex::new(fds));

            let conf = Arc::new(pingora::server::configuration::ServerConf::default());
            let mut svc = pingora::proxy::http_proxy_service_with_name(
                &conf,
                shared_router,
                "Neemata Proxy (HTTP)",
            );
            if let Some(settings) = tls_settings {
                svc.add_tls_with_settings(&listen, None, settings);
            } else {
                svc.add_tcp(&listen);
            }

            let task = tokio::spawn(async move {
                let (shutdown_tx, shutdown_rx) = watch::channel(false);
                let mut service_fut = Box::pin(async move {
                    use pingora::services::Service as _;
                    #[cfg(unix)]
                    svc.start_service(Some(listen_fds), shutdown_rx, conf.listener_tasks_per_fd)
                        .await;

                    #[cfg(windows)]
                    svc.start_service(shutdown_rx, conf.listener_tasks_per_fd)
                        .await;
                });

                tokio::select! {
                    _ = &mut service_fut => {}
                    _ = cancel_child.cancelled() => {
                        let _ = shutdown_tx.send(true);
                        let _ = service_fut.await;
                    }
                }
            });

            server
                .upsert_service(
                    "main_http".to_string(),
                    server::ServiceHandle { cancel, task },
                )
                .await;

            Ok(())
        })
    }

    #[napi]
    pub fn stop<'env>(&self, env: &'env Env) -> Result<PromiseRaw<'env, ()>> {
        let (server, inner) = {
            let mut guard = self
                .inner
                .lock()
                .map_err(|_| napi::Error::from_reason("Proxy mutex poisoned"))?;

            match guard.state {
                ProxyState::Stopped => {
                    return env.spawn_future(async { Ok(()) });
                }
                ProxyState::Stopping => {
                    return env.spawn_future(async { Ok(()) });
                }
                ProxyState::Starting | ProxyState::Running => {
                    guard.state = ProxyState::Stopping;
                }
            }

            (guard.server.clone(), self.inner.clone())
        };

        env.spawn_future(async move {
            server.stop_all().await;
            if let Ok(mut guard) = inner.lock() {
                guard.state = ProxyState::Stopped;
            }
            Ok(())
        })
    }

    #[napi]
    pub fn add_upstream<'env>(
        &self,
        env: &'env Env,
        app_name: String,
        upstream: UpstreamOptions,
    ) -> Result<PromiseRaw<'env, ()>> {
        let upstream = parse_port_upstream(env, upstream)?;

        let (server, is_running, to_upsert, to_remove) = {
            let mut guard = self
                .inner
                .lock()
                .map_err(|_| napi::Error::from_reason("Proxy mutex poisoned"))?;

            let app_def = guard
                .options
                .applications
                .iter()
                .find(|a| a.name == app_name)
                .cloned();
            let health_check_interval_ms = guard.options.health_check_interval_ms;
            let Some(app_def) = app_def else {
                return Err(errors::generic_error(
                    env,
                    errors::codes::UNKNOWN_APPLICATION,
                    format!("Unknown application '{app_name}'"),
                ));
            };

            let upstreams = guard
                .upstreams_by_app
                .get_mut(&app_name)
                .expect("apps are initialized at construction");

            if upstreams.contains(&upstream) {
                return Err(errors::generic_error(
                    env,
                    errors::codes::UPSTREAM_ALREADY_EXISTS,
                    "Upstream already exists",
                ));
            }

            // TLS policy: if this upstream is secure, we must be able to derive verify hostname.
            if upstream.secure {
                let _ = verify_hostname_for_secure_upstream(env, &app_def)?;
            }

            upstreams.insert(upstream);

            let (new_pools, removed_transports) =
                rebuild_app_pools(env, &app_def, upstreams, health_check_interval_ms)?;

            let mut to_upsert = Vec::new();
            let mut to_remove = Vec::new();

            for (t, p) in new_pools {
                to_upsert.push((lb_service_name(&app_name, t), p.lb.clone()));
                guard.pools.insert((app_name.clone(), t), p);
            }
            for t in removed_transports {
                if guard.pools.remove(&(app_name.clone(), t)).is_some() {
                    to_remove.push(lb_service_name(&app_name, t));
                }
            }

            // Always update router config (even when stopped) so it is ready for `start()`.
            let cfg = build_router_config(&guard.options, &guard.pools);
            guard.router.update(cfg);

            (
                guard.server.clone(),
                guard.state == ProxyState::Running,
                to_upsert,
                to_remove,
            )
        };

        if !is_running {
            return env.spawn_future(async { Ok(()) });
        }

        env.spawn_future(async move {
            for (name, lb) in to_upsert {
                server
                    .upsert_service(name, lb::spawn_lb_health_task(lb))
                    .await;
            }
            for name in to_remove {
                server.remove_service(&name).await;
            }
            Ok(())
        })
    }

    #[napi]
    pub fn remove_upstream<'env>(
        &self,
        env: &'env Env,
        app_name: String,
        upstream: UpstreamOptions,
    ) -> Result<PromiseRaw<'env, ()>> {
        let upstream = parse_port_upstream(env, upstream)?;

        let (server, is_running, to_upsert, to_remove) = {
            let mut guard = self
                .inner
                .lock()
                .map_err(|_| napi::Error::from_reason("Proxy mutex poisoned"))?;

            let app_def = guard
                .options
                .applications
                .iter()
                .find(|a| a.name == app_name)
                .cloned();
            let health_check_interval_ms = guard.options.health_check_interval_ms;
            let Some(app_def) = app_def else {
                return Err(errors::generic_error(
                    env,
                    errors::codes::UNKNOWN_APPLICATION,
                    format!("Unknown application '{app_name}'"),
                ));
            };

            let upstreams = guard
                .upstreams_by_app
                .get_mut(&app_name)
                .expect("apps are initialized at construction");

            if !upstreams.remove(&upstream) {
                return Err(errors::generic_error(
                    env,
                    errors::codes::UPSTREAM_NOT_FOUND,
                    "Upstream not found",
                ));
            }

            let (new_pools, removed_transports) =
                rebuild_app_pools(env, &app_def, upstreams, health_check_interval_ms)?;

            let mut to_upsert = Vec::new();
            let mut to_remove = Vec::new();

            for (t, p) in new_pools {
                to_upsert.push((lb_service_name(&app_name, t), p.lb.clone()));
                guard.pools.insert((app_name.clone(), t), p);
            }
            for t in removed_transports {
                if guard.pools.remove(&(app_name.clone(), t)).is_some() {
                    to_remove.push(lb_service_name(&app_name, t));
                }
            }

            let cfg = build_router_config(&guard.options, &guard.pools);
            guard.router.update(cfg);

            (
                guard.server.clone(),
                guard.state == ProxyState::Running,
                to_upsert,
                to_remove,
            )
        };

        if !is_running {
            return env.spawn_future(async { Ok(()) });
        }

        env.spawn_future(async move {
            for (name, lb) in to_upsert {
                server
                    .upsert_service(name, lb::spawn_lb_health_task(lb))
                    .await;
            }
            for name in to_remove {
                server.remove_service(&name).await;
            }
            Ok(())
        })
    }
}

fn build_router_config(
    options: &options::ProxyOptionsParsed,
    pools: &HashMap<(String, lb::TransportKind), router::PoolConfig>,
) -> router::RouterConfig {
    let mut cfg = router::RouterConfig::default();

    for app in &options.applications {
        match &app.routing {
            options::ApplicationRoutingParsed::Subdomain { name } => {
                cfg.subdomain_routes
                    .insert(name.to_ascii_lowercase(), app.name.clone());
            }
            options::ApplicationRoutingParsed::Path { name } => {
                cfg.path_routes.insert(name.clone(), app.name.clone());
            }
            options::ApplicationRoutingParsed::Default => {
                cfg.default_app = Some(app.name.clone());
            }
        }

        let http1 = pools
            .get(&(app.name.clone(), lb::TransportKind::Http1))
            .cloned();
        let ws = pools
            .get(&(app.name.clone(), lb::TransportKind::Ws))
            .cloned();
        let http2 = pools
            .get(&(app.name.clone(), lb::TransportKind::Http2))
            .cloned();
        cfg.apps
            .insert(app.name.clone(), router::AppPools { http1, ws, http2 });
    }

    cfg
}

fn lb_service_name(app: &str, transport: lb::TransportKind) -> String {
    match transport {
        lb::TransportKind::Http1 => format!("lb:{app}:http"),
        lb::TransportKind::Http2 => format!("lb:{app}:http2"),
        lb::TransportKind::Ws => format!("lb:{app}:ws"),
    }
}

fn parse_port_upstream(env: &Env, upstream: UpstreamOptions) -> Result<PortUpstream> {
    match upstream {
        Either::A(port) => {
            if port.r#type != "port" {
                return Err(errors::generic_error(
                    env,
                    errors::codes::UNSUPPORTED_UPSTREAM_TYPE,
                    format!("Unsupported upstream type '{}'", port.r#type),
                ));
            }

            let transport = match port.transport.as_str() {
                "http" => lb::TransportKind::Http1,
                "http2" => lb::TransportKind::Http2,
                "ws" => lb::TransportKind::Ws,
                other => {
                    return errors::throw_type_error(
                        env,
                        errors::codes::INVALID_PROXY_OPTIONS,
                        format!("Unknown upstream transport '{other}'"),
                    );
                }
            };

            if port.port == 0 || port.port > u16::MAX as u32 {
                return errors::throw_type_error(
                    env,
                    errors::codes::INVALID_PROXY_OPTIONS,
                    "PortUpstreamOptions.port must be in range 1..=65535",
                );
            }

            if port.hostname.is_empty() {
                return errors::throw_type_error(
                    env,
                    errors::codes::INVALID_PROXY_OPTIONS,
                    "PortUpstreamOptions.hostname must be non-empty",
                );
            }

            Ok(PortUpstream {
                transport,
                secure: port.secure,
                hostname: port.hostname,
                port: port.port as u16,
            })
        }
        Either::B(unix) => {
            if unix.r#type != "unix" && unix.r#type != "unix_socket" {
                return Err(errors::generic_error(
                    env,
                    errors::codes::UNSUPPORTED_UPSTREAM_TYPE,
                    format!("Unsupported upstream type '{}'", unix.r#type),
                ));
            }
            Err(errors::generic_error(
                env,
                errors::codes::UNSUPPORTED_UPSTREAM_TYPE,
                "unix_socket upstreams are not supported",
            ))
        }
    }
}

fn verify_hostname_for_secure_upstream(
    env: &Env,
    app: &options::ApplicationOptionsParsed,
) -> Result<String> {
    match &app.routing {
        options::ApplicationRoutingParsed::Subdomain { name } => Ok(name.clone()),
        options::ApplicationRoutingParsed::Path { .. }
        | options::ApplicationRoutingParsed::Default => {
            let Some(sni) = &app.sni else {
                return Err(errors::type_error(
                    env,
                    errors::codes::INVALID_APPLICATION_OPTIONS,
                    "ApplicationOptions.sni is required when using secure upstreams for path/default routing",
                ));
            };
            Ok(sni.clone())
        }
    }
}

fn rebuild_app_pools(
    env: &Env,
    app: &options::ApplicationOptionsParsed,
    upstreams: &HashSet<PortUpstream>,
    health_check_interval_ms: u32,
) -> Result<(
    HashMap<lb::TransportKind, router::PoolConfig>,
    Vec<lb::TransportKind>,
)> {
    let mut by_transport: HashMap<lb::TransportKind, Vec<&PortUpstream>> = HashMap::new();
    for u in upstreams {
        by_transport.entry(u.transport).or_default().push(u);
    }

    let mut new_pools = HashMap::new();

    for (transport, list) in by_transport {
        if list.is_empty() {
            continue;
        }

        let secure = list[0].secure;
        if list.iter().any(|u| u.secure != secure) {
            return Err(errors::type_error(
                env,
                errors::codes::INVALID_PROXY_OPTIONS,
                "Mixing secure and insecure upstreams within the same (app, transport) pool is not supported",
            ));
        }

        let verify_hostname = if secure {
            verify_hostname_for_secure_upstream(env, app)?
        } else {
            String::new()
        };

        let mut addrs = Vec::new();
        for u in &list {
            let iter = (u.hostname.as_str(), u.port)
                .to_socket_addrs()
                .map_err(|e| {
                    errors::type_error(
                        env,
                        errors::codes::INVALID_PROXY_OPTIONS,
                        format!(
                            "Failed to resolve upstream '{}:{}': {e}",
                            u.hostname, u.port
                        ),
                    )
                })?;
            addrs.extend(iter);
        }

        if addrs.is_empty() {
            continue;
        }

        let backends = Arc::new(
            addrs
                .iter()
                .map(std::string::ToString::to_string)
                .collect::<HashSet<_>>(),
        );

        let mut lb = LoadBalancer::<RoundRobin>::try_from_iter(addrs).map_err(|e| {
            errors::type_error(
                env,
                errors::codes::INVALID_PROXY_OPTIONS,
                format!("Failed to build load balancer: {e}"),
            )
        })?;

        if secure {
            lb.set_health_check(TcpHealthCheck::new_tls(&verify_hostname));
        } else {
            lb.set_health_check(TcpHealthCheck::new());
        }

        // TODO(vNext): Define dynamic-upstream readiness semantics.
        // Currently, a newly added upstream becomes eligible only after the LB health-check loop
        // runs and marks it healthy. This means `addUpstream()` does not imply "immediately routable".
        // Options:
        // - optimistic: treat newly-added backends as healthy until the first failed check/connection
        // - eager: run an initial health check synchronously during addUpstream/start, then serve
        // - explicit: expose readiness state/events to JS so callers can await health convergence
        // Acceptance: behavior is documented, deterministic, and covered by integration tests.
        lb.health_check_frequency = Some(Duration::from_millis(health_check_interval_ms as u64));

        new_pools.insert(
            transport,
            router::PoolConfig {
                lb: Arc::new(lb),
                secure,
                verify_hostname,
                backends,
            },
        );
    }

    let mut removed = Vec::new();
    for t in [
        lb::TransportKind::Http1,
        lb::TransportKind::Http2,
        lb::TransportKind::Ws,
    ] {
        if !new_pools.contains_key(&t) {
            removed.push(t);
        }
    }

    Ok((new_pools, removed))
}
