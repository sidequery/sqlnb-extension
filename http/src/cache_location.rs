#[cfg(any(target_os = "linux", all(test, unix)))]
use std::ffi::OsStr;
use std::path::PathBuf;
use url::Url;

pub(super) fn asset_cache_scope(remote: &Url) -> Result<PathBuf, String> {
    Ok(cache_root()?
        .join("sidequery/sqlnb-extension/ui")
        .join(crate::revision(remote.as_str().as_bytes())))
}

pub(super) fn notebook_state_path(root: &std::path::Path) -> Result<PathBuf, String> {
    Ok(cache_root()?
        .join("sidequery/sqlnb-extension/notebooks")
        .join(crate::revision(root.as_os_str().as_encoded_bytes()))
        .join("recent-notebook.json"))
}

fn cache_root() -> Result<PathBuf, String> {
    #[cfg(target_os = "macos")]
    let root = PathBuf::from(
        std::env::var_os("HOME").ok_or("HOME is unavailable; cannot locate UI cache")?,
    )
    .join("Library/Caches");
    #[cfg(target_os = "linux")]
    let root = linux_cache_root(std::env::var_os("XDG_CACHE_HOME").as_deref(), || {
        std::env::var_os("HOME")
    })?;
    #[cfg(windows)]
    let root = windows_local_data()?.join("Cache");
    Ok(root)
}

#[cfg(any(target_os = "linux", all(test, unix)))]
fn linux_cache_root(
    xdg_cache_home: Option<&OsStr>,
    home: impl FnOnce() -> Option<std::ffi::OsString>,
) -> Result<PathBuf, String> {
    if let Some(path) = xdg_cache_home
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
    {
        return Ok(path);
    }
    // Empty or relative XDG paths are not usable cache roots. HOME is needed
    // only for the fallback, including in containers that intentionally omit it.
    let home = home()
        .filter(|home| !home.is_empty())
        .ok_or("HOME is unavailable and XDG_CACHE_HOME is not absolute; cannot locate UI cache")?;
    Ok(PathBuf::from(home).join(".cache"))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn absolute_xdg_cache_does_not_read_home() {
        assert_eq!(
            linux_cache_root(Some(OsStr::new("/var/cache/sqlnb")), || panic!(
                "HOME must not be read"
            ))
            .unwrap(),
            PathBuf::from("/var/cache/sqlnb")
        );
    }

    #[test]
    fn missing_empty_and_relative_xdg_use_home() {
        for xdg in [
            None,
            Some(OsStr::new("")),
            Some(OsStr::new("relative/cache")),
        ] {
            assert_eq!(
                linux_cache_root(xdg, || Some("/home/user".into())).unwrap(),
                PathBuf::from("/home/user/.cache")
            );
        }
    }

    #[test]
    fn missing_home_is_only_an_error_without_absolute_xdg() {
        for xdg in [None, Some(OsStr::new("")), Some(OsStr::new("relative"))] {
            assert!(linux_cache_root(xdg, || None).is_err());
            assert!(linux_cache_root(xdg, || Some("".into())).is_err());
        }
    }
}

#[cfg(windows)]
pub(super) fn windows_local_data() -> Result<PathBuf, String> {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .ok_or_else(|| "LOCALAPPDATA is unavailable or not absolute".into())
}
