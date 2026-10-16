//! Fetches profile pictures and server icons from Discord's CDN on a thread of its own, a few at a
//! time, keeping them on disk: a hash never changes content, so nothing is ever fetched twice.

use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::Arc,
    thread,
    time::SystemTime,
};

use bardo_api::Cdn;
use tokio::sync::{Semaphore, mpsc};
use tracing::{debug, error, warn};

use crate::snapshot::CdnImage;

/// Downloads at once.
const CONCURRENCY: usize = 4;
/// The disk cache is trimmed to this size when the fetcher starts, oldest files first.
const CACHE_LIMIT: u64 = 20 * 1024 * 1024;

type Done = Box<dyn FnOnce(Option<Vec<u8>>) + Send>;

struct Job {
    url: String,
    file: PathBuf,
    done: Done,
}

/// Stops when dropped; downloads under way are abandoned.
pub struct CdnFetcher {
    jobs: mpsc::UnboundedSender<Job>,
}

impl CdnFetcher {
    /// Starts the fetcher with its disk cache in `dir`.
    pub fn start(dir: PathBuf) -> Self {
        let (jobs, receiver) = mpsc::unbounded_channel();
        let spawned = thread::Builder::new().name("bardo-cdn".into()).spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(runtime) => runtime,
                Err(e) => return error!("failed to start the CDN runtime: {e}"),
            };
            runtime.block_on(run(dir, receiver));
        });
        if let Err(e) = spawned {
            error!("failed to spawn the CDN thread: {e}");
        }
        Self { jobs }
    }

    /// Fetches `image` at `size` pixels and calls `done` with its bytes, or `None` if it could not
    /// be had, on a background thread where decoding it is fine.
    pub fn fetch(&self, image: &CdnImage, size: u32, done: impl FnOnce(Option<Vec<u8>>) + Send + 'static) {
        let job = Job { url: image.url(size), file: PathBuf::from(image.file_name(size)), done: Box::new(done) };
        if let Err(mpsc::error::SendError(job)) = self.jobs.send(job) {
            (job.done)(None);
        }
    }
}

async fn run(dir: PathBuf, mut jobs: mpsc::UnboundedReceiver<Job>) {
    let cdn = match Cdn::new() {
        Ok(cdn) => Arc::new(cdn),
        Err(e) => return error!("failed to build the CDN client: {e}"),
    };
    {
        let dir = dir.clone();
        tokio::task::spawn_blocking(move || {
            if let Err(e) = fs::create_dir_all(&dir).and_then(|()| trim(&dir, CACHE_LIMIT)) {
                warn!("could not tidy the image cache: {e}");
            }
        });
    }
    let permits = Arc::new(Semaphore::new(CONCURRENCY));
    while let Some(job) = jobs.recv().await {
        let (cdn, permits, path) = (cdn.clone(), permits.clone(), dir.join(&job.file));
        tokio::spawn(async move {
            let Ok(_permit) = permits.acquire().await else { return };
            let bytes = load(&cdn, &job.url, &path).await;
            let done = job.done;
            tokio::task::spawn_blocking(move || done(bytes));
        });
    }
}

/// From the disk cache, or downloaded into it.
async fn load(cdn: &Cdn, url: &str, path: &Path) -> Option<Vec<u8>> {
    if let Ok(bytes) = tokio::fs::read(path).await {
        return Some(bytes);
    }
    let bytes = match cdn.get(url).await {
        Ok(bytes) => bytes,
        Err(e) => {
            debug!("could not fetch {url}: {e}");
            return None;
        }
    };
    // Written beside it and renamed, so a half-written file is never read back.
    let temp = path.with_extension("tmp");
    let written = tokio::fs::write(&temp, &bytes).await.and(tokio::fs::rename(&temp, path).await);
    if let Err(e) = written {
        debug!("could not cache {}: {e}", path.display());
    }
    Some(bytes)
}

/// Deletes the oldest cached pictures until the rest fit in `limit` bytes.
fn trim(dir: &Path, limit: u64) -> io::Result<()> {
    let mut files: Vec<(SystemTime, u64, PathBuf)> = fs::read_dir(dir)?
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let metadata = entry.metadata().ok().filter(fs::Metadata::is_file)?;
            Some((metadata.modified().ok()?, metadata.len(), entry.path()))
        })
        .collect();
    let mut total: u64 = files.iter().map(|(_, len, _)| len).sum();
    files.sort();
    for (_, len, path) in files {
        if total <= limit {
            break;
        }
        fs::remove_file(&path)?;
        total -= len;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{sync::mpsc as std_mpsc, time::Duration};

    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("bardo-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn trimming_deletes_the_oldest_first() {
        let dir = temp_dir("cdn-trim");
        for (index, name) in ["old", "middle", "new"].iter().enumerate() {
            let path = dir.join(format!("{name}-64.webp"));
            fs::write(&path, [0u8; 100]).unwrap();
            let file = fs::File::options().write(true).open(&path).unwrap();
            file.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(1_000 + index as u64)).unwrap();
        }
        trim(&dir, 250).unwrap();
        let mut left: Vec<String> =
            fs::read_dir(&dir).unwrap().map(|entry| entry.unwrap().file_name().into_string().unwrap()).collect();
        left.sort();
        assert_eq!(left, ["middle-64.webp", "new-64.webp"]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn cached_pictures_are_not_downloaded_again() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let dir = temp_dir("cdn-cache");
        let image = CdnImage::avatar(bardo_gateway::Id(1), "cafe");
        fs::write(dir.join(image.file_name(64)), b"cached").unwrap();
        let fetcher = CdnFetcher::start(dir.clone());
        let (sender, receiver) = std_mpsc::channel();
        fetcher.fetch(&image, 64, move |bytes| sender.send(bytes).unwrap());
        assert_eq!(receiver.recv_timeout(Duration::from_secs(5)).unwrap().as_deref(), Some(&b"cached"[..]));
        drop(fetcher);
        fs::remove_dir_all(&dir).unwrap();
    }
}
