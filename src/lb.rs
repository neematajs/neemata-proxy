use std::{net::SocketAddr, sync::Arc, time::Duration};

use pingora::lb::{LoadBalancer, selection::RoundRobin};
use pingora::server::ShutdownWatch;
use pingora::services::background::BackgroundService;
use tokio::sync::watch;

use crate::server::ServiceHandle;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[allow(dead_code)]
pub enum TransportKind {
    Http1,
    Http2,
}

#[allow(dead_code)]
pub fn build_round_robin_lb(
    addrs: Vec<SocketAddr>,
    health_check_frequency: Option<Duration>,
) -> std::io::Result<Arc<LoadBalancer<RoundRobin>>> {
    let mut lb = LoadBalancer::<RoundRobin>::try_from_iter(addrs)?;
    lb.health_check_frequency = health_check_frequency;
    Ok(Arc::new(lb))
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
