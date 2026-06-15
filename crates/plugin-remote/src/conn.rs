//! A single connection to an out-of-process plugin that exposes several
//! components (a provider and/or multiple named drivers) over one socket.
//!
//! All derived handles share the one mux, so every component multiplexes over
//! the same connection; each request carries the component name so the guest
//! routes it. This matches a real plugin like plugin-go (one process: a
//! provider plus the golist/embed/testmain drivers).

use crate::driver::RemoteDriver;
use crate::host::{HostCallbackHandler, HostInner};
use crate::managed::RemoteManagedDriver;
use crate::provider::RemoteProvider;
use plugin_abi::mux::Mux;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite};

pub struct RemotePlugin {
    mux: Arc<Mux>,
    inner: Arc<HostInner>,
}

impl RemotePlugin {
    /// Open a plugin connection over an established duplex byte stream.
    pub fn connect<R, W>(read: R, write: W) -> Self
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let inner = Arc::new(HostInner::default());
        let handler = Arc::new(HostCallbackHandler {
            inner: Arc::clone(&inner),
        });
        let mux = Mux::start(read, write, handler);
        Self { mux, inner }
    }

    /// A handle to this plugin's provider (`name` is its registered name).
    pub fn provider(&self, name: impl Into<String>) -> RemoteProvider {
        RemoteProvider::from_parts(Arc::clone(&self.mux), Arc::clone(&self.inner), name.into())
    }

    /// A handle to one of this plugin's managed drivers, selected by `name`.
    pub fn managed_driver(&self, name: impl Into<String>) -> RemoteManagedDriver {
        RemoteManagedDriver::from_parts(Arc::clone(&self.mux), Arc::clone(&self.inner), name.into())
    }

    /// A handle to one of this plugin's (non-managed) drivers, selected by `name`.
    pub fn driver(&self, name: impl Into<String>) -> RemoteDriver {
        RemoteDriver::from_parts(Arc::clone(&self.mux), Arc::clone(&self.inner), name.into())
    }
}
