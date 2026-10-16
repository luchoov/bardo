//! Installing a downloaded update: the installer replaces an installed bardo, a portable one
//! replaces its own exe. Either way bardo closes and the new version starts.

use std::{
    fs, io,
    path::{Path, PathBuf},
    process::Command,
};

use bardo_core::Package;
use tracing::warn;

/// Passed to the new exe after a portable update, with the old process id: wait for it to exit.
pub const AFTER_UPDATE_ARG: &str = "--after-update";
/// Inno Setup leaves its uninstaller next to an installed bardo.
const UNINSTALLER: &str = "unins000.exe";
/// Where a portable update moves the running exe, which cannot be deleted until it exits.
const OLD_EXE: &str = "bardo.old.exe";
const NEW_EXE: &str = "bardo.new.exe";
/// Under the temp folder.
const INSTALLER_DIR: &str = "bardo-update";
const INSTALLER: &str = "bardo-setup.exe";
/// No wizard, only a progress window; closes bardo if it is somehow still running and starts it
/// again when done.
const INSTALLER_ARGS: [&str; 4] = ["/SILENT", "/SUPPRESSMSGBOXES", "/NORESTART", "/CLOSEAPPLICATIONS"];

/// An update ready to take over once bardo has closed.
pub enum Pending {
    Installer(PathBuf),
    /// The exe is already replaced; the new one only needs starting.
    Portable(PathBuf),
}

/// What to download: the installer if bardo was installed with it, the exe otherwise.
pub fn package() -> Package {
    let installed = exe_dir().is_some_and(|dir| dir.join(UNINSTALLER).is_file());
    if installed { Package::Installer } else { Package::Portable }
}

/// Puts the downloaded `bytes` in place.
pub fn prepare(package: Package, bytes: &[u8]) -> io::Result<Pending> {
    match package {
        Package::Installer => {
            let dir = std::env::temp_dir().join(INSTALLER_DIR);
            fs::create_dir_all(&dir)?;
            let installer = dir.join(INSTALLER);
            fs::write(&installer, bytes)?;
            Ok(Pending::Installer(installer))
        }
        Package::Portable => {
            let exe = std::env::current_exe()?;
            let dir = exe.parent().ok_or(io::ErrorKind::NotFound)?;
            let (old, new) = (dir.join(OLD_EXE), dir.join(NEW_EXE));
            fs::write(&new, bytes)?;
            remove_if_present(&old)?;
            // A running exe can be renamed but not overwritten.
            fs::rename(&exe, &old)?;
            if let Err(e) = fs::rename(&new, &exe) {
                let _ = fs::rename(&old, &exe);
                let _ = fs::remove_file(&new);
                return Err(e);
            }
            Ok(Pending::Portable(exe))
        }
    }
}

/// Starts the update. Called last, as bardo exits.
pub fn launch(pending: Pending) {
    let spawned = match &pending {
        Pending::Installer(installer) => Command::new(installer).args(INSTALLER_ARGS).spawn(),
        Pending::Portable(exe) => Command::new(exe).arg(AFTER_UPDATE_ARG).arg(std::process::id().to_string()).spawn(),
    };
    if let Err(e) = spawned {
        warn!("failed to start the update: {e}");
    }
}

/// Removes what an earlier update left behind.
pub fn clean_up() {
    if let Some(dir) = exe_dir() {
        let _ = fs::remove_file(dir.join(OLD_EXE));
    }
    // The installer may still be finishing; then this waits for the next start.
    let _ = fs::remove_dir_all(std::env::temp_dir().join(INSTALLER_DIR));
}

/// The process id an updated bardo was started with, to wait for.
pub fn replaced_process() -> Option<u32> {
    let mut args = std::env::args().skip_while(|arg| arg != AFTER_UPDATE_ARG).skip(1);
    args.next()?.parse().ok()
}

fn exe_dir() -> Option<PathBuf> {
    std::env::current_exe().ok()?.parent().map(Path::to_path_buf)
}

fn remove_if_present(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}
