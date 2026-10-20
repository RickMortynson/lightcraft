//! Receive macOS Open Documents events through a safe, bounded queue.
//!
//! Install on the main thread before the native event loop, then drain during normal app logic.
//! This isolated helper owns the small Objective-C bridge required by winit 0.30's missing
//! Open Documents support. See the crate README for the lifecycle contract.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use std::path::PathBuf;

/// A request handed to the normal application import flow, or an invalid native request.
pub type OpenRequest = Result<Vec<PathBuf>, String>;

/// A main-thread native receiver. Keep it alive while the window event loop runs.
pub struct OpenDocuments {
    #[cfg(target_os = "macos")]
    inner: macos::Receiver,
}

impl OpenDocuments {
    /// Register the native handler before starting the window loop. `wake` requests an app
    /// frame; it may do nothing until the UI context exists. Queued requests survive that gap.
    pub fn install(wake: impl Fn() + Send + Sync + 'static) -> Result<Self, String> {
        #[cfg(target_os = "macos")]
        {
            macos::Receiver::install(wake).map(|inner| Self { inner })
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = wake;
            Err("Open Documents Apple Events are available only on macOS".into())
        }
    }

    /// Take pending requests in arrival order. This never reads the documents themselves.
    pub fn drain(&self) -> Vec<OpenRequest> {
        #[cfg(target_os = "macos")]
        {
            self.inner.drain()
        }
        #[cfg(not(target_os = "macos"))]
        {
            Vec::new()
        }
    }
}

#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
mod macos;
