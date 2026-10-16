//! Downloading a newer bardo, on its own thread.

use std::thread;

use bardo_api::{Package, Release};
use tracing::error;

#[derive(Debug)]
pub enum UpdateEvent {
    /// The fraction downloaded so far.
    Progress(f32),
    /// The whole file, checked against the release's checksums.
    Downloaded(Vec<u8>),
    Failed(String),
}

/// Downloads `package` from `release`. `on_event` is called from the download thread, and last
/// with `Downloaded` or `Failed`.
pub fn download_update(release: Release, package: Package, mut on_event: impl FnMut(UpdateEvent) + Send + 'static) {
    let spawned = thread::Builder::new().name("bardo-update".into()).spawn(move || {
        let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
            Ok(runtime) => runtime,
            Err(e) => return on_event(UpdateEvent::Failed(format!("failed to start the update runtime: {e}"))),
        };
        let downloaded =
            runtime.block_on(bardo_api::download_release(&release, package, |done| on_event(UpdateEvent::Progress(done))));
        on_event(match downloaded {
            Ok(bytes) => UpdateEvent::Downloaded(bytes),
            Err(e) => UpdateEvent::Failed(e.to_string()),
        });
    });
    if let Err(e) = spawned {
        error!("failed to spawn the update thread: {e}");
    }
}
