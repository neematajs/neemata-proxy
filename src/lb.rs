use std::{collections::BTreeSet, sync::Arc};

use futures::FutureExt;
use pingora::lb::{Backend, Backends, LoadBalancer, discovery, selection::RoundRobin};
use pingora::server::ShutdownWatch;
use pingora::services::background::BackgroundService;
use tokio::sync::watch;

use crate::server::ServiceHandle;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[allow(dead_code)]
pub enum TransportKind {
    Http1,
    Http2,
    Ws,
}

pub fn build_round_robin_lb(
    backends: BTreeSet<Backend>,
) -> std::io::Result<LoadBalancer<RoundRobin>> {
    let discovery = discovery::Static::new(backends);
    let backends = Backends::new(discovery);
    let lb = LoadBalancer::<RoundRobin>::from_backends(backends);

    lb.update()
        .now_or_never()
        .expect("static backend discovery should not block")
        .map_err(|e| std::io::Error::other(format!("failed to update load balancer: {e}")))?;

    Ok(lb)
}

#[allow(dead_code)]
pub fn spawn_lb_health_task(lb: Arc<LoadBalancer<RoundRobin>>) -> ServiceHandle {
    let cancel = tokio_util::sync::CancellationToken::new();
    let cancel_child = cancel.clone();

    let (shutdown_tx, shutdown_rx): (watch::Sender<bool>, ShutdownWatch) = watch::channel(false);

    let task = tokio::spawn(async move {
        let start_fut = lb.start(shutdown_rx);
        tokio::select! {
            _ = start_fut => {}
            _ = cancel_child.cancelled() => {
                let _ = shutdown_tx.send(true);
            }
        }
    });

    ServiceHandle { cancel, task }
}
