//! OS-specific file operations behind the notebook root capability.
#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;
#[cfg(unix)]
pub(crate) use unix::*;
#[cfg(windows)]
pub(crate) use windows::*;

#[derive(PartialEq, Eq)]
pub(crate) struct FileIdentity {
    pub volume: u64,
    pub index: u64,
    pub regular: bool,
}

pub(crate) fn home_directory() -> Option<std::ffi::OsString> {
    #[cfg(windows)]
    let home = std::env::var_os("USERPROFILE");
    #[cfg(unix)]
    let home = std::env::var_os("HOME");
    home.filter(|value| !value.is_empty())
}

pub(crate) fn valid_component(name: &std::ffi::OsStr) -> bool {
    if name.as_encoded_bytes().eq_ignore_ascii_case(b".git") {
        return false;
    }
    #[cfg(windows)]
    {
        let Some(name) = name.to_str() else {
            return false;
        };
        if name.ends_with(['.', ' ']) || name.chars().any(|c| c < ' ' || ":<>\"|?*".contains(c)) {
            return false;
        }
        let stem = name.split('.').next().unwrap_or("").to_ascii_uppercase();
        if stem
            .strip_prefix("GIT~")
            .is_some_and(|suffix| !suffix.is_empty() && suffix.bytes().all(|c| c.is_ascii_digit()))
            || matches!(
                stem.as_str(),
                "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
            )
            || ["COM", "LPT"].iter().any(|prefix| {
                stem.strip_prefix(prefix).is_some_and(|suffix| {
                    matches!(
                        suffix,
                        "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
                    )
                })
            })
        {
            return false;
        }
    }
    true
}

pub(crate) fn sync_history_path(path: &std::path::Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        std::fs::File::open(path)?.sync_all()
    }
    #[cfg(windows)]
    {
        let metadata = std::fs::symlink_metadata(path)?;
        if is_reparse(&metadata) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "reparse history path",
            ));
        }
        if metadata.is_dir() || metadata.permissions().readonly() {
            // Windows cannot flush directories or reopen immutable Git objects
            // for GENERIC_WRITE. libgit2 flushes these objects before making
            // them readonly (core.fsyncObjectFiles is enabled by History).
            return Ok(());
        }
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)?
            .sync_all()
    }
}
