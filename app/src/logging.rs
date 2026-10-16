//! Logging. A release build has no console, so the log goes to `%LOCALAPPDATA%\bardo\bardo.log`,
//! which is what to attach to a bug report; debug builds also print it. Panics are logged too:
//! without that, a release build that panics just disappears.

use std::{
    fs::{self, File},
    io::{self, Write},
    os::windows::fs::OpenOptionsExt,
    panic,
    path::PathBuf,
    sync::Mutex,
};

use tracing::error;
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};

const FILE_SHARE_READ: u32 = 0x1;
const FILE_SHARE_WRITE: u32 = 0x2;
/// A log larger than this at startup is kept as `bardo.old.log` and a new one is started.
const ROTATE_AT: u64 = 1024 * 1024;
/// Writing stops at this size, so a runaway loop cannot fill the disk.
const MAX_SIZE: u64 = 20 * 1024 * 1024;

/// Where the log files are.
pub fn dir() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|dir| PathBuf::from(dir).join("bardo"))
}

pub fn init() {
    let file = open().map(|file| fmt::layer().with_ansi(false).with_writer(Mutex::new(file)));
    let console = cfg!(debug_assertions).then(fmt::layer);
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(file)
        .with(console)
        .init();

    let default_hook = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current();
        error!("thread '{}' {info}", thread.name().unwrap_or("unnamed"));
        default_hook(info);
    }));
}

fn open() -> Option<CappedFile> {
    let dir = dir()?;
    fs::create_dir_all(&dir).ok()?;
    let path = dir.join("bardo.log");
    if fs::metadata(&path).is_ok_and(|metadata| metadata.len() > ROTATE_AT) {
        // Fails while another instance has it open; it is rotated next time.
        let _ = fs::rename(&path, dir.join("bardo.old.log"));
    }
    // Shared for reading and writing but not deleting, so a second instance cannot rotate the log
    // out from under the one writing it.
    let file = File::options()
        .create(true)
        .append(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .open(&path)
        .ok()?;
    let written = file.metadata().map_or(0, |metadata| metadata.len());
    Some(CappedFile { file, written })
}

/// A file that silently stops growing at [`MAX_SIZE`].
struct CappedFile {
    file: File,
    written: u64,
}

impl Write for CappedFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.written >= MAX_SIZE {
            return Ok(buf.len());
        }
        let written = self.file.write(buf)?;
        self.written += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}
