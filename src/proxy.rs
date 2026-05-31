use crate::{errors, lb, options, router, server};
use napi::bindgen_prelude::execute_tokio_future;
use napi::bindgen_prelude::*;
use napi_derive::napi;
use pingora::lb::{LoadBalancer, health_check::TcpHealthCheck, selection::RoundRobin};
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::net::{SocketAddr, ToSocketAddrs};
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

#[derive(Debug, Clone)]
enum StartStatus {
    Pending,
    Ready(std::result::Result<(), String>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StopStatus {
    Pending,
    Ready,
}

#[derive(Debug, Clone)]
struct DeferredJsError {
    code: &'static str,
    message: String,
}

type DeferredResult<T> = std::result::Result<T, DeferredJsError>;

impl DeferredJsError {
    fn generic(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    fn type_error(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

struct ProxyInner {
    options: options::ProxyOptionsParsed,
    state: ProxyState,
    listen_address: Option<BoundListenAddress>,
    start_notifier: Option<watch::Sender<StartStatus>>,
    stop_notifier: Option<watch::Sender<StopStatus>>,
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct BoundListenAddress {
    hostname: String,
    port: u16,
}

#[napi(object)]
pub struct ProxyAddress {
    pub hostname: String,
    pub port: u32,
}

impl From<SocketAddr> for BoundListenAddress {
    fn from(addr: SocketAddr) -> Self {
        Self {
            hostname: addr.ip().to_string(),
            port: addr.port(),
        }
    }
}

impl From<&BoundListenAddress> for ProxyAddress {
    fn from(address: &BoundListenAddress) -> Self {
        Self {
            hostname: address.hostname.clone(),
            port: address.port as u32,
        }
    }
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
    update_lock: Arc<TokioMutex<()>>,
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
            listen_address: None,
            start_notifier: None,
            stop_notifier: None,
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
            update_lock: Arc::new(TokioMutex::new(())),
        })
    }

    #[napi]
    pub fn start<'env>(&self, env: &'env Env) -> Result<PromiseRaw<'env, ()>> {
        let (listen, tls, server, inner, shared_router, initial_lb_tasks, mut start_rx) = {
            let mut guard = self
                .inner
                .lock()
                .map_err(|_| napi::Error::from_reason("Proxy mutex poisoned"))?;

            match guard.state {
                ProxyState::Starting => {
                    let Some(notifier) = guard.start_notifier.as_ref() else {
                        return Err(napi::Error::from_reason(
                            "Proxy internal error: missing start notifier",
                        ));
                    };

                    let mut rx = notifier.subscribe();
                    return env
                        .spawn_future(async move { Self::wait_for_start_ready(&mut rx).await });
                }
                ProxyState::Running => {
                    return Err(errors::generic_error(
                        env,
                        errors::codes::ALREADY_STARTED,
                        "Proxy is already started",
                    ));
                }
                ProxyState::Stopping => {
                    return Err(errors::generic_error(
                        env,
                        errors::codes::ALREADY_STARTED,
                        "Proxy is stopping",
                    ));
                }
                ProxyState::Stopped => {}
            }

            guard.state = ProxyState::Starting;
            guard.listen_address = None;
            let (start_notifier, start_rx) = watch::channel(StartStatus::Pending);
            guard.start_notifier = Some(start_notifier);

            let listen = guard.options.listen.clone();
            let tls = guard.options.tls.clone();
            let server = guard.server.clone();
            let inner = self.inner.clone();
            let shared_router = router::SharedRouter::new(guard.router.clone());
            let initial_lb_tasks = guard
                .pools
                .iter()
                .map(|((app, transport), pool)| (lb_service_name(app, *transport), pool.lb.clone()))
                .collect::<Vec<_>>();

            (
                listen,
                tls,
                server,
                inner,
                shared_router,
                initial_lb_tasks,
                start_rx,
            )
        };

        // Move listener setup and service start behind the async boundary, then resolve once
        // startup is fully wired.
        spawn_deferred_result(env, async move {
            let listen_for_setup = listen.clone();
            let setup_result =
                tokio::task::spawn_blocking(move || build_start_resources(&listen_for_setup, tls))
                    .await
                    .map_err(|e| {
                        napi::Error::from_reason(format!(
                            "Proxy start task failed during listener setup: {e}"
                        ))
                    })?;

            let (std_listener, tls_settings, listen_address) = match setup_result {
                Ok(resources) => resources,
                Err(err) => {
                    if let Ok(mut guard) = inner.lock() {
                        guard.state = ProxyState::Stopped;
                        guard.listen_address = None;
                        if let Some(notifier) = guard.start_notifier.take() {
                            let _ = notifier.send(StartStatus::Ready(Err(err.message.clone())));
                        }
                    }

                    return Ok(Err(err));
                }
            };

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
                    {
                        // Windows artifacts are not currently published. This fallback lets
                        // Pingora bind from `listen`, so `listen: ...:0` is not guaranteed to
                        // match `listen_address`; ephemeral-port reporting is supported on Unix.
                        svc.start_service(shutdown_rx, conf.listener_tasks_per_fd)
                            .await;
                    }
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

            let start_result = {
                let mut guard = inner
                    .lock()
                    .map_err(|_| napi::Error::from_reason("Proxy mutex poisoned"))?;

                let result = match guard.state {
                    ProxyState::Starting | ProxyState::Running => {
                        guard.state = ProxyState::Running;
                        guard.listen_address = Some(listen_address);
                        Ok(())
                    }
                    ProxyState::Stopping | ProxyState::Stopped => {
                        Err("Proxy start was cancelled while stopping".to_string())
                    }
                };

                if let Some(notifier) = guard.start_notifier.take() {
                    let _ = notifier.send(StartStatus::Ready(result.clone()));
                }

                result
            };

            if start_result.is_err() {
                server.stop_all().await;
                let mut guard = inner
                    .lock()
                    .map_err(|_| napi::Error::from_reason("Proxy mutex poisoned"))?;
                guard.state = ProxyState::Stopped;
                guard.listen_address = None;
                return Err(napi::Error::from_reason(
                    "Proxy start was cancelled while stopping",
                ));
            }

            Self::wait_for_start_ready(&mut start_rx).await?;
            Ok(Ok(()))
        })
    }

    #[napi]
    pub fn stop<'env>(&self, env: &'env Env) -> Result<PromiseRaw<'env, ()>> {
        let (server, inner, mut start_rx) = {
            let mut guard = self
                .inner
                .lock()
                .map_err(|_| napi::Error::from_reason("Proxy mutex poisoned"))?;

            match guard.state {
                ProxyState::Stopped => {
                    guard.listen_address = None;
                    return env.spawn_future(async { Ok(()) });
                }
                ProxyState::Stopping => {
                    let Some(notifier) = guard.stop_notifier.as_ref() else {
                        return Err(napi::Error::from_reason(
                            "Proxy internal error: missing stop notifier",
                        ));
                    };

                    let mut rx = notifier.subscribe();
                    return env
                        .spawn_future(async move { Self::wait_for_stop_ready(&mut rx).await });
                }
                ProxyState::Starting | ProxyState::Running => {
                    guard.state = ProxyState::Stopping;
                    if guard.stop_notifier.is_none() {
                        let (stop_notifier, _) = watch::channel(StopStatus::Pending);
                        guard.stop_notifier = Some(stop_notifier);
                    }
                }
            }

            (
                guard.server.clone(),
                self.inner.clone(),
                guard.start_notifier.as_ref().map(watch::Sender::subscribe),
            )
        };

        env.spawn_future(async move {
            if let Some(rx) = start_rx.as_mut() {
                let _ = Self::wait_for_start_ready(rx).await;
            }

            server.stop_all().await;
            if let Ok(mut guard) = inner.lock() {
                guard.state = ProxyState::Stopped;
                guard.listen_address = None;
                guard.start_notifier = None;
                if let Some(notifier) = guard.stop_notifier.take() {
                    let _ = notifier.send(StopStatus::Ready);
                }
            }
            Ok(())
        })
    }

    async fn wait_for_start_ready(rx: &mut watch::Receiver<StartStatus>) -> Result<()> {
        loop {
            let status = rx.borrow().clone();
            match status {
                StartStatus::Pending => {
                    if rx.changed().await.is_err() {
                        return Err(napi::Error::from_reason(
                            "Proxy start channel closed unexpectedly",
                        ));
                    }
                }
                StartStatus::Ready(Ok(())) => return Ok(()),
                StartStatus::Ready(Err(msg)) => return Err(napi::Error::from_reason(msg)),
            }
        }
    }

    async fn wait_for_stop_ready(rx: &mut watch::Receiver<StopStatus>) -> Result<()> {
        loop {
            let status = *rx.borrow();
            match status {
                StopStatus::Pending => {
                    if rx.changed().await.is_err() {
                        return Err(napi::Error::from_reason(
                            "Proxy stop channel closed unexpectedly",
                        ));
                    }
                }
                StopStatus::Ready => return Ok(()),
            }
        }
    }

    #[napi]
    pub fn address(&self) -> Result<Option<ProxyAddress>> {
        let guard = self
            .inner
            .lock()
            .map_err(|_| napi::Error::from_reason("Proxy mutex poisoned"))?;

        if guard.state != ProxyState::Running {
            return Ok(None);
        }

        Ok(guard.listen_address.as_ref().map(ProxyAddress::from))
    }

    #[napi]
    pub fn add_upstream<'env>(
        &self,
        env: &'env Env,
        app_name: String,
        upstream: UpstreamOptions,
    ) -> Result<PromiseRaw<'env, ()>> {
        let upstream = parse_port_upstream(env, upstream)?;

        let inner = self.inner.clone();
        let update_lock = self.update_lock.clone();

        spawn_deferred_result(env, async move {
            let _update_guard = update_lock.lock().await;

            let (server, app_def, health_check_interval_ms, mut next_upstreams) = {
                let guard = inner
                    .lock()
                    .map_err(|_| napi::Error::from_reason("Proxy mutex poisoned"))?;

                let Some(app_def) = guard
                    .options
                    .applications
                    .iter()
                    .find(|a| a.name == app_name)
                    .cloned()
                else {
                    return Ok(Err(DeferredJsError::generic(
                        errors::codes::UNKNOWN_APPLICATION,
                        format!("Unknown application '{app_name}'"),
                    )));
                };

                let Some(current_upstreams) = guard.upstreams_by_app.get(&app_name) else {
                    return Ok(Err(DeferredJsError::generic(
                        errors::codes::UNKNOWN_APPLICATION,
                        format!("Unknown application '{app_name}'"),
                    )));
                };

                if current_upstreams.contains(&upstream) {
                    return Ok(Err(DeferredJsError::generic(
                        errors::codes::UPSTREAM_ALREADY_EXISTS,
                        "Upstream already exists",
                    )));
                }

                (
                    guard.server.clone(),
                    app_def,
                    guard.options.health_check_interval_ms,
                    current_upstreams.clone(),
                )
            };

            next_upstreams.insert(upstream.clone());

            if upstream.secure
                && let Err(err) = verify_hostname_for_secure_upstream(&app_def)
            {
                return Ok(Err(err));
            }

            let rebuilt = tokio::task::spawn_blocking({
                let app_def = app_def.clone();
                let next_upstreams = next_upstreams.clone();
                move || rebuild_app_pools(&app_def, &next_upstreams, health_check_interval_ms)
            })
            .await
            .map_err(|e| {
                napi::Error::from_reason(format!(
                    "Upstream rebuild task failed while adding upstream: {e}"
                ))
            })?;

            let (new_pools, removed_transports) = match rebuilt {
                Ok(pools) => pools,
                Err(err) => return Ok(Err(err)),
            };

            let (is_running, to_upsert, to_remove) = {
                let mut guard = inner
                    .lock()
                    .map_err(|_| napi::Error::from_reason("Proxy mutex poisoned"))?;

                let Some(upstreams) = guard.upstreams_by_app.get_mut(&app_name) else {
                    return Ok(Err(DeferredJsError::generic(
                        errors::codes::UNKNOWN_APPLICATION,
                        format!("Unknown application '{app_name}'"),
                    )));
                };
                *upstreams = next_upstreams;

                let mut to_upsert = Vec::new();
                let mut to_remove = Vec::new();

                for (transport, pool) in new_pools {
                    to_upsert.push((lb_service_name(&app_name, transport), pool.lb.clone()));
                    guard.pools.insert((app_name.clone(), transport), pool);
                }
                for transport in removed_transports {
                    if guard.pools.remove(&(app_name.clone(), transport)).is_some() {
                        to_remove.push(lb_service_name(&app_name, transport));
                    }
                }

                let cfg = build_router_config(&guard.options, &guard.pools);
                guard.router.update(cfg);

                (guard.state == ProxyState::Running, to_upsert, to_remove)
            };

            if is_running {
                for (name, lb) in to_upsert {
                    server
                        .upsert_service(name, lb::spawn_lb_health_task(lb))
                        .await;
                }
                for name in to_remove {
                    server.remove_service(&name).await;
                }
            }

            Ok(Ok(()))
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

        let inner = self.inner.clone();
        let update_lock = self.update_lock.clone();

        spawn_deferred_result(env, async move {
            let _update_guard = update_lock.lock().await;

            let (server, app_def, health_check_interval_ms, mut next_upstreams) = {
                let guard = inner
                    .lock()
                    .map_err(|_| napi::Error::from_reason("Proxy mutex poisoned"))?;

                let Some(app_def) = guard
                    .options
                    .applications
                    .iter()
                    .find(|a| a.name == app_name)
                    .cloned()
                else {
                    return Ok(Err(DeferredJsError::generic(
                        errors::codes::UNKNOWN_APPLICATION,
                        format!("Unknown application '{app_name}'"),
                    )));
                };

                let Some(current_upstreams) = guard.upstreams_by_app.get(&app_name) else {
                    return Ok(Err(DeferredJsError::generic(
                        errors::codes::UNKNOWN_APPLICATION,
                        format!("Unknown application '{app_name}'"),
                    )));
                };

                if !current_upstreams.contains(&upstream) {
                    return Ok(Err(DeferredJsError::generic(
                        errors::codes::UPSTREAM_NOT_FOUND,
                        "Upstream not found",
                    )));
                }

                (
                    guard.server.clone(),
                    app_def,
                    guard.options.health_check_interval_ms,
                    current_upstreams.clone(),
                )
            };

            next_upstreams.remove(&upstream);

            let rebuilt = tokio::task::spawn_blocking({
                let app_def = app_def.clone();
                let next_upstreams = next_upstreams.clone();
                move || rebuild_app_pools(&app_def, &next_upstreams, health_check_interval_ms)
            })
            .await
            .map_err(|e| {
                napi::Error::from_reason(format!(
                    "Upstream rebuild task failed while removing upstream: {e}"
                ))
            })?;

            let (new_pools, removed_transports) = match rebuilt {
                Ok(pools) => pools,
                Err(err) => return Ok(Err(err)),
            };

            let (is_running, to_upsert, to_remove) = {
                let mut guard = inner
                    .lock()
                    .map_err(|_| napi::Error::from_reason("Proxy mutex poisoned"))?;

                let Some(upstreams) = guard.upstreams_by_app.get_mut(&app_name) else {
                    return Ok(Err(DeferredJsError::generic(
                        errors::codes::UNKNOWN_APPLICATION,
                        format!("Unknown application '{app_name}'"),
                    )));
                };
                *upstreams = next_upstreams;

                let mut to_upsert = Vec::new();
                let mut to_remove = Vec::new();

                for (transport, pool) in new_pools {
                    to_upsert.push((lb_service_name(&app_name, transport), pool.lb.clone()));
                    guard.pools.insert((app_name.clone(), transport), pool);
                }
                for transport in removed_transports {
                    if guard.pools.remove(&(app_name.clone(), transport)).is_some() {
                        to_remove.push(lb_service_name(&app_name, transport));
                    }
                }

                let cfg = build_router_config(&guard.options, &guard.pools);
                guard.router.update(cfg);

                (guard.state == ProxyState::Running, to_upsert, to_remove)
            };

            if is_running {
                for (name, lb) in to_upsert {
                    server
                        .upsert_service(name, lb::spawn_lb_health_task(lb))
                        .await;
                }
                for name in to_remove {
                    server.remove_service(&name).await;
                }
            }

            Ok(Ok(()))
        })
    }
}

fn spawn_deferred_result<'env, F>(env: &'env Env, fut: F) -> Result<PromiseRaw<'env, ()>>
where
    F: 'static + Send + Future<Output = Result<DeferredResult<()>>>,
{
    let promise = execute_tokio_future(env.raw(), fut, |raw_env, result| {
        let env = Env::from_raw(raw_env);
        let settled = match result {
            Ok(()) => PromiseRaw::resolve(&env, ())?,
            Err(err) => {
                let error = napi::Error::new(err.code.to_string(), err.message);
                PromiseRaw::<()>::reject(&env, error)?
            }
        };

        Ok(settled.raw())
    })?;

    Ok(PromiseRaw::new(env.raw(), promise))
}

fn build_start_resources(
    listen: &str,
    tls: Option<options::ListenerTlsOptionsParsed>,
) -> DeferredResult<(
    std::net::TcpListener,
    Option<pingora::listeners::tls::TlsSettings>,
    BoundListenAddress,
)> {
    let std_listener = std::net::TcpListener::bind(listen).map_err(|e| {
        DeferredJsError::generic(
            errors::codes::LISTEN_BIND_FAILED,
            format!("Failed to bind listener on '{listen}': {e}"),
        )
    })?;

    let listen_address = std_listener.local_addr().map_err(|e| {
        DeferredJsError::generic(
            errors::codes::LISTEN_BIND_FAILED,
            format!("Failed to read bound listener address for '{listen}': {e}"),
        )
    })?;

    std_listener.set_nonblocking(true).map_err(|e| {
        DeferredJsError::generic(
            errors::codes::LISTEN_BIND_FAILED,
            format!("Failed to set listener nonblocking: {e}"),
        )
    })?;

    let tls_settings = if let Some(tls) = tls {
        let mut settings =
            pingora::listeners::tls::TlsSettings::intermediate(&tls.cert_path, &tls.key_path)
                .map_err(|e| {
                    DeferredJsError::type_error(
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

    Ok((std_listener, tls_settings, listen_address.into()))
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
    app: &options::ApplicationOptionsParsed,
) -> DeferredResult<String> {
    match &app.routing {
        options::ApplicationRoutingParsed::Subdomain { name } => Ok(name.clone()),
        options::ApplicationRoutingParsed::Path { .. }
        | options::ApplicationRoutingParsed::Default => {
            let Some(sni) = &app.sni else {
                return Err(DeferredJsError::type_error(
                    errors::codes::INVALID_APPLICATION_OPTIONS,
                    "ApplicationOptions.sni is required when using secure upstreams for path/default routing",
                ));
            };
            Ok(sni.clone())
        }
    }
}

fn rebuild_app_pools(
    app: &options::ApplicationOptionsParsed,
    upstreams: &HashSet<PortUpstream>,
    health_check_interval_ms: u32,
) -> DeferredResult<(
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
            return Err(DeferredJsError::type_error(
                errors::codes::INVALID_PROXY_OPTIONS,
                "Mixing secure and insecure upstreams within the same (app, transport) pool is not supported",
            ));
        }

        let verify_hostname = if secure {
            verify_hostname_for_secure_upstream(app)?
        } else {
            String::new()
        };

        let mut addrs = Vec::new();
        for u in &list {
            let iter = (u.hostname.as_str(), u.port)
                .to_socket_addrs()
                .map_err(|e| {
                    DeferredJsError::type_error(
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
            return Err(DeferredJsError::type_error(
                errors::codes::INVALID_PROXY_OPTIONS,
                format!(
                    "Failed to resolve upstream '{}:{}': no addresses were returned",
                    list[0].hostname, list[0].port
                ),
            ));
        }

        let backends = Arc::new(
            addrs
                .iter()
                .map(std::string::ToString::to_string)
                .collect::<HashSet<_>>(),
        );

        let mut lb = LoadBalancer::<RoundRobin>::try_from_iter(addrs).map_err(|e| {
            DeferredJsError::type_error(
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
