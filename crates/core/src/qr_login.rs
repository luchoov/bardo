//! Signing in with a QR code, on its own thread while the sign-in screen is up.

use std::thread;

use tokio::sync::oneshot;
use tracing::error;

pub use bardo_api::RemoteAuthEvent as QrLoginEvent;

/// Shows QR codes to scan with the Discord app, one after another as they expire, until one signs
/// in. Stops when dropped.
pub struct QrLogin {
    shutdown: Option<oneshot::Sender<()>>,
}

impl QrLogin {
    /// `on_event` is called from the sign-in thread.
    pub fn start(on_event: impl FnMut(QrLoginEvent) + Send + 'static) -> Self {
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let spawned = thread::Builder::new().name("bardo-qr-login".into()).spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(runtime) => runtime,
                Err(e) => return error!("failed to start the QR sign-in runtime: {e}"),
            };
            runtime.block_on(async move {
                tokio::select! {
                    _ = shutdown_rx => {}
                    _ = async {
                        let properties = bardo_api::client_properties().await;
                        bardo_api::remote_auth(&properties, on_event).await
                    } => {}
                }
            });
        });
        if let Err(e) = spawned {
            error!("failed to spawn the QR sign-in thread: {e}");
        }
        Self { shutdown: Some(shutdown_tx) }
    }
}

impl Drop for QrLogin {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }
}
