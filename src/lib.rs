#![deny(clippy::all)]

mod errors;
mod lb;
mod options;
mod proxy;
mod router;
mod server;

#[doc(hidden)]
pub mod bench {
    pub use crate::router::bench::*;
}

pub use proxy::Proxy;
