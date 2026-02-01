# Specification: Neemata Proxy (NAPI-RS + Pingora)

Status: Final (implementation-ready)
Target: next breaking version of `@nmtjs/proxy`

This document is the normative spec for the vNext proxy implementation.

---

## 1. Goals / Non-goals

### Goals

1. Provide a Node.js-facing `Proxy` class (NAPI) with:
   - `start()` that resolves only after the listener bind attempt completes (success or failure).
   - `stop()` that stops all services and waits for them to exit.
   - `addUpstream(app, upstream)` / `removeUpstream(app, upstream)` that dynamically updates routing.
2. Allow `addUpstream/removeUpstream` both before and after `start()`.
3. Keep the request routing read path lock-free (use `ArcSwap`).
4. Keep application definitions immutable after construction (only upstream sets change).
5. Provide stable JS error `code` strings for all failures.
6. Provide a static downstream “middleware” extension point via Pingora HTTP modules (configured once at startup).

### Non-goals

1. Dynamic create/remove of applications after construction.
2. OS signal handling inside Rust (delegated to Node.js).
3. Unix socket upstreams behind load balancing (Pingora LB backends are INET-only).
4. L7 health checks (vNext uses TCP health checks only).
5. WebSockets over HTTP/2 (RFC8441) and WebSocket frame/message inspection.
6. Dynamically adding/removing downstream modules after the proxy starts serving.

---

## 2. Hard constraints (Pingora / runtime)

1. Pingora load balancing `Backend` supports only INET socket addresses (no UDS).
2. Pingora’s standard listening `Service` uses `expect(...)` on listener build, making bind failures unrecoverable/panic if used directly.

Implication: vNext MUST own listener binding so bind failures surface to JS as `ListenBindFailed` without panicking.

---

## 3. Public JavaScript API (normative)

### 3.1 `Proxy` class

```ts
export class Proxy {
  constructor(options: ProxyOptions);

  start(): Promise<void>;
  stop(): Promise<void>;

  addUpstream(appName: string, upstream: PortUpstreamOptions | UnixSocketUpstreamOptions): Promise<void>;
  removeUpstream(appName: string, upstream: PortUpstreamOptions | UnixSocketUpstreamOptions): Promise<void>;
}
```

### 3.2 Options

```ts
export type ProxyOptions = {
  listen: string; // e.g. "0.0.0.0:3000"
  tls?: ListenerTlsOptions;
  applications: ApplicationOptions[];
  // Health check interval in milliseconds. Defaults to 5000 (5 seconds).
  healthCheckIntervalMs?: number;
};

export type ListenerTlsOptions = {
  certPath: string; // PEM chain
  keyPath: string;  // PEM private key
  enableH2?: boolean; // default false
};

export type ApplicationOptions = {
  name: string;
  routing:
    | { type: "subdomain"; name: string } // exact host match, case-insensitive, port ignored
    | { type: "path"; name: string }      // first path segment match ("/auth/..." => "auth")
    | { default: true };                   // fallback

  // Upstream TLS SNI/verification hostname override.
  // Required for `path` and `default` routes when any upstream is `secure: true`.
  sni?: string;
};

export type PortUpstreamOptions = {
  type: "port";
  // - "http": HTTP/1.1 to upstream (supports Upgrade/WebSocket)
  // - "http2": HTTP/2 to upstream (no Upgrade; RFC8441 out of scope)
  transport: "http" | "http2";
  secure: boolean;
  hostname: string;
  port: number;
};

export type UnixSocketUpstreamOptions = {
  type: "unix_socket";
  transport: "http" | "http2";
  secure: boolean;
  path: string;
};
```

### 3.3 JS-visible errors

All thrown/rejected errors MUST include:
- `message: string`
- `code: string` (stable)

Minimum stable error codes:
- `InvalidProxyOptions`
- `InvalidApplicationOptions`
- `UnknownApplication`
- `AlreadyStarted`
- `ListenBindFailed`
- `UnsupportedUpstreamType`
- `UpstreamAlreadyExists`
- `UpstreamNotFound`

---

## 4. Observable semantics

### 4.1 Start/Stop state machine

States: `Stopped` | `Starting` | `Running` | `Stopping`

Rules:
1. `start()`
   - If `Stopped`: transition to `Starting`, spawn listener task, wait for readiness.
   - If `Starting`: MUST NOT spawn a second listener; MUST await the same readiness.
   - If `Running`: reject with `AlreadyStarted`.
   - If readiness resolves `Ok`: transition to `Running` and resolve.
   - If readiness resolves `Err`: transition back to `Stopped` and reject with `ListenBindFailed`.
2. `stop()`
   - If `Stopped`: resolve immediately.
   - If `Starting`: request shutdown, wait for listener exit, transition to `Stopped`, resolve.
   - If `Running`: request shutdown for all services, wait for exit, transition to `Stopped`, resolve.
   - If `Stopping`: wait for completion and resolve.

### 4.2 `addUpstream/removeUpstream`

1. The application list is fixed at construction time.
2. Unknown `appName` MUST reject with `UnknownApplication`.
3. Calls MUST work in both `Stopped` and `Running`.
4. Updates for a single app MUST be serialized to avoid interleaving concurrent changes.
5. The listener MUST NOT restart due to routing changes.

---

## 5. Routing

### 5.1 Application invariants

For each application, `routing` MUST be one of:
- `{ type: "subdomain", name }`
- `{ type: "path", name }`
- `{ default: true }`

Additional invariants:
- At most one default app (`routing: { default: true }`).
- For `{ type: "path" }`: `name` MUST be non-empty and MUST NOT contain `/`.
- For `{ type: "subdomain" }`: `name` MUST be non-empty.

### 5.2 Matching & precedence (explicit choice)

Precedence: `subdomain` → `path` → `default`.

- Subdomain match: compare request host case-insensitively; ignore port.
- Path match: match the first non-empty path segment.
- Default: used only if no subdomain or path match.

### 5.3 Path rewriting for `path` apps

If the matched path segment is `auth`, rewrite upstream path by stripping the first segment:
- `/auth` → `/`
- `/auth/` → `/`
- `/auth/login?x=1` → `/login?x=1`

If the first segment does not match exactly (segment boundary-aware), no rewrite occurs.

### 5.4 WebSocket / Upgrade

- WebSocket is an HTTP/1.1 Upgrade mechanism.
- WebSockets over HTTP/2 (RFC8441) are out of scope.

Behavior:
1. If the downstream request is an upgrade request, the proxy MUST choose an upstream from the app’s `transport: "http"` pool.
2. If the app has no `transport: "http"` upstreams available, the request MUST fail (implementation-defined HTTP error response).
3. `transport: "http2"` upstreams are used only for non-upgrade requests.

---

## 6. Upstreams

### 6.1 Internal model

```rust
pub enum TransportKind { Http1, Http2 }

pub enum Upstream {
  Port { hostname: String, port: u16, secure: bool, transport: TransportKind },
  Unix { path: std::path::PathBuf, secure: bool, transport: TransportKind },
}
```

### 6.2 Identity / deduplication

Uniqueness key:
- Port: `(hostname, port, secure, transport)`
- Unix: `(path, secure, transport)`

Adding an upstream with an existing key MUST reject with `UpstreamAlreadyExists`.
Removing a missing upstream MUST reject with `UpstreamNotFound`.

### 6.3 Unix socket upstreams

Because Pingora LB backends are INET-only:
- `addUpstream(..., { type: "unix_socket", ... })` MUST reject with `UnsupportedUpstreamType`.
- `removeUpstream(..., { type: "unix_socket", ... })` MUST reject with `UnsupportedUpstreamType`.

---

## 7. TLS policy

### 7.1 Inbound TLS

If `ProxyOptions.tls` is present:
- Listener MUST serve TLS using `certPath`/`keyPath`.
- If `enableH2` is true, ALPN MUST include `h2`.

If absent: listener is plain TCP (HTTP/1.1).

### 7.2 Upstream TLS and safe SNI/verification

Security rule: upstream TLS identity MUST NOT be derived from client-controlled inputs.

When an upstream has `secure: true`:
- TLS MUST be enabled.
- Certificate verification MUST be enabled.
- Verify hostname MUST be chosen per application:
  - `routing.type == "subdomain"`: verify hostname = `routing.name`.
  - `routing.type == "path"`: requires `ApplicationOptions.sni`; verify hostname = `sni`.
  - `routing.default == true`: requires `ApplicationOptions.sni`; verify hostname = `sni`.

If any secure upstream exists but verify hostname cannot be determined, treat as invalid config (`InvalidApplicationOptions`).

When `secure: false`, no upstream SNI/verification is performed.

---

## 8. Internal architecture (normative shape)

### 8.1 Core Pingora components and responsibilities

- `pingora_proxy::HttpProxy<Router>` is the downstream HTTP proxy engine.
- `Router` implements `pingora_proxy::ProxyHttp` and is responsible for:
  - routing to an app (`subdomain → path → default`),
  - choosing the pool (Upgrade => H1 only; otherwise by transport),
  - selecting a `Backend` from the chosen `LoadBalancer`,
  - producing a `pingora_core::upstreams::peer::HttpPeer`.
- One `pingora_load_balancing::LoadBalancer<selection::RoundRobin>` per `(app, transport)`.
- TCP health checks via `pingora_load_balancing::health_check::TcpHealthCheck`.

### 8.2 Downstream modules (`HttpModules`)

`Router` MUST register downstream modules exactly once at startup by overriding:

```rust
fn init_downstream_modules(&self, modules: &mut pingora_core::modules::http::HttpModules)
```

Policy:
- Modules are internal-only (not exposed to JS in vNext).
- Module registration is static at startup (no runtime add/remove).
- Keep Pingora’s default compression module present but disabled (compression level 0).

### 8.3 Router configuration & updates (ArcSwap)

- `Router` holds `ArcSwap<RouterConfig>`.
- `Router` reads MUST be lock-free.
- Updates MUST replace the entire `RouterConfig` atomically.

Recommended shape:

```rust
pub struct Router { config: arc_swap::ArcSwap<RouterConfig> }

pub struct RouterConfig { apps: std::collections::HashMap<String, Application> }

pub struct Application {
  route: AppRoute,
  balancer_h1: std::sync::Arc<pingora_load_balancing::LoadBalancer<pingora_load_balancing::selection::RoundRobin>>,
  balancer_h2: std::sync::Arc<pingora_load_balancing::LoadBalancer<pingora_load_balancing::selection::RoundRobin>>,
  verify_hostname: Option<String>,
}

pub enum AppRoute { Subdomain { host: String }, Path { segment: String }, Default }
```

### 8.4 Custom service manager (`Server`)

We do NOT use `pingora::server::Server`. We use a small Tokio-based service manager.

```rust
pub struct Server {
  services: std::sync::Arc<tokio::sync::Mutex<std::collections::HashMap<String, ServiceHandle>>>,
}

pub struct ServiceHandle {
  cancel: tokio_util::sync::CancellationToken,
  task: tokio::task::JoinHandle<()>,
}
```

API requirements:
- `add_service(name, service)` fails if exists.
- `upsert_service(name, service)` MUST atomically replace old with new by cancelling + awaiting old.
- All services MUST honor cancellation.

---

## 9. Dynamic updates & health checks

### 9.1 Update algorithm (add/remove)

For both `addUpstream` and `removeUpstream`:
1. Validate app exists (else `UnknownApplication`).
2. Reject unsupported upstream type `unix_socket` (`UnsupportedUpstreamType`).
3. Serialize updates per app.
4. Build a NEW `LoadBalancer` for the affected `(app, transport)` pool.
5. Replace the corresponding health-check background service using `Server::upsert_service`.
6. Swap `RouterConfig` to reference the new `LoadBalancer`.

### 9.2 Health checks (TCP)

- Health checks are TCP-only.
- Health-check task MUST run against the exact `LoadBalancer` instance referenced by active `RouterConfig`.
- Health check interval is configurable via `ProxyOptions.healthCheckIntervalMs` (default: 5000ms).

Implementation sketch (wiring shape only):

```rust
fn spawn_lb_health_task(
  name: String,
  lb: std::sync::Arc<pingora_load_balancing::LoadBalancer<pingora_load_balancing::selection::RoundRobin>>,
) -> ServiceHandle {
  let cancel = tokio_util::sync::CancellationToken::new();
  let cancel_child = cancel.clone();
  let (shutdown_tx, shutdown_watch) = tokio::sync::watch::channel(false);

  let task = tokio::spawn(async move {
    tokio::select! {
      _ = cancel_child.cancelled() => { let _ = shutdown_tx.send(true); }
    }
    lb.start(shutdown_watch).await;
    drop(name);
  });

  ServiceHandle { cancel, task }
}
```

TODO(vNext+1): expose enable/disable + threshold knobs for health checks.

---

## 10. Listener & readiness

### 10.1 Requirement

`start()` MUST resolve only after the listener bind attempt completes (success or failure).

### 10.2 Readiness handshake

- Listener task reports readiness exactly once via `tokio::sync::oneshot`.
- Success => `start()` resolves.
- Bind error => `start()` rejects with `ListenBindFailed`.

### 10.3 Listener implementation (open decision)

We must avoid Pingora’s panic-on-bind-failure listener service.

Open decision (choose one during implementation):
- Option A (recommended): custom Tokio listener (and TLS stack as needed) that hands accepted connections into Pingora’s `HttpProxy` engine.
- Option B: use Pingora builders but avoid the `expect(...)` bind path (only if a no-panic bind API is confirmed).

Chosen strategy MUST satisfy:
- No panic on bind failure.
- Bind result returned via readiness handshake.
- Cancellation stops accept loop.

---

## 11. Acceptance criteria

1. `start()` rejects with `AlreadyStarted` when called while `Running`.
2. `start()` rejects with `ListenBindFailed` when the address cannot be bound.
3. Concurrent `start()` calls in `Starting` do not spawn multiple listeners and all observe the same readiness outcome.
4. `stop()` is idempotent and resolves from any state.
5. `addUpstream/removeUpstream` work before `start()` and affect the state observed after starting.
6. Unknown app name => `UnknownApplication`.
7. Adding duplicate upstream => `UpstreamAlreadyExists`; removing missing => `UpstreamNotFound`.
8. Any `unix_socket` upstream operation => `UnsupportedUpstreamType`.
9. Routing precedence is `subdomain` → `path` → `default`.
10. Path apps rewrite upstream paths by stripping the first segment.
11. Secure upstreams never derive verify hostname from downstream host/SNI; they follow §7.2.
12. Upgrade/WebSocket requests only use `transport: "http"` upstreams; RFC8441 is out of scope.
13. Downstream module registration happens exactly once at startup via `init_downstream_modules`; modules are not dynamically added/removed at runtime.

---

## 12. Future work (TODO)

### 12.1 Unix Domain Socket (UDS) support

Currently UDS is unsupported due to Pingora LB `Backend` being INET-only (§2, §6.3).

#### 12.1.1 Downstream listener on UDS

Allow `ProxyOptions.listen` to accept a Unix socket path (e.g., `/var/run/proxy.sock`).

Requirements:
- Detect listen type (INET vs UDS) from the `listen` string format.
- Bind a `tokio::net::UnixListener` instead of `TcpListener`.
- Handle socket file lifecycle (remove stale socket, set permissions).
- Same readiness/error semantics as TCP: `ListenBindFailed` on bind failure.

#### 12.1.2 Upstream UDS support

Allow `UnixSocketUpstreamOptions` to work.

Challenge: Pingora's `LoadBalancer<RoundRobin>` and `Backend` are INET-only.

Potential approaches:
1. **Bypass LB for UDS**: Maintain a separate pool of UDS upstreams per app, do round-robin manually, produce `HttpPeer` with `Unix` socket address directly.
2. **Contribute to Pingora**: Add `Backend` variant for UDS (unlikely to be accepted upstream).
3. **Single UDS upstream per app**: Simplify to at most one UDS upstream (no LB needed), reject if more than one.

Requirements when implemented:
- Remove `UnsupportedUpstreamType` error for `unix_socket`.
- TCP health checks may need adaptation (or skip health checks for UDS).
- Ensure `Upstream` identity works correctly with `(path, secure, transport)`.
