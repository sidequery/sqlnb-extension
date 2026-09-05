#[cfg(test)]
use axum::body::Body;
use axum::{
    body::to_bytes,
    extract::{DefaultBodyLimit, Query, Request, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
#[cfg(test)]
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    ffi::{c_char, c_void, CStr, CString},
    fs::{self, File},
    io::{Read, Write},
    net::{Ipv4Addr, SocketAddrV4, TcpListener},
    os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd},
    os::unix::ffi::OsStringExt,
    os::unix::fs::MetadataExt,
    path::{Component, Path, PathBuf},
    ptr,
    sync::{mpsc, Arc, Mutex},
    thread,
    time::Duration,
};
use subtle::ConstantTimeEq;
use tokio::sync::watch;
use url::Url;

#[cfg(not(unix))]
compile_error!("sidequery-ui-http currently supports macOS and Linux");

const MAX_QUACK_MESSAGE_BYTES: usize = 64 * 1024 * 1024;
const MAX_NOTEBOOK_BYTES: usize = 16 * 1024 * 1024;
const CSP: &str = "default-src 'self'; script-src 'self' 'wasm-unsafe-eval' 'unsafe-eval'; style-src 'self' 'unsafe-inline'; img-src 'self' data: blob:; font-src 'self'; connect-src 'self'; worker-src 'self' blob:; object-src 'none'; base-uri 'none'; frame-ancestors 'none'; form-action 'none'";

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ServerConfig {
    port: u16,
    local_asset_directory: String,
    remote_asset_url: String,
    quack_url: String,
    token: String,
    duckdb_version: String,
    #[serde(default)]
    notebook_root: String,
}

#[derive(Clone)]
struct AppState {
    port: u16,
    origin: String,
    token: String,
    duckdb_version: String,
    quack_url: Url,
    client: reqwest::Client,
    assets: AssetSource,
    notebooks: Arc<NotebookStore>,
}

#[derive(Clone)]
enum AssetSource {
    Local(PathBuf),
    Remote(Url),
}

struct NotebookStore {
    root: PathBuf,
    root_fd: OwnedFd,
    writes: Mutex<()>,
}

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
}

impl ApiError {
    fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = Json(serde_json::json!({"error": {"code": self.code, "message": self.message}}));
        (self.status, body).into_response()
    }
}

struct ServerHandle {
    port: u16,
    shutdown: Option<watch::Sender<bool>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Drop for ServerHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

impl ServerHandle {
    fn stop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(true);
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct BootstrapResponse {
    bridge_version: u8,
    protocol: &'static str,
    endpoint: &'static str,
    token: String,
    supports_cancel: bool,
    engine: &'static str,
    duckdb_version: String,
}

#[derive(Serialize)]
struct FileEntry {
    path: String,
}

#[derive(Serialize)]
struct FilesResponse {
    root: String,
    files: Vec<FileEntry>,
    directories: Vec<String>,
}

#[derive(Deserialize)]
struct FileQuery {
    path: String,
}

#[derive(Serialize)]
struct FileResponse {
    path: String,
    source: String,
    revision: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PutFileRequest {
    path: String,
    source: String,
    expected_revision: String,
}

#[derive(Serialize)]
struct PutFileResponse {
    path: String,
    revision: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MoveFileRequest {
    path: String,
    destination_path: String,
    expected_revision: Option<String>,
}

#[derive(Serialize)]
struct MoveFileResponse {
    path: String,
    revision: String,
}

#[derive(Deserialize)]
struct CreateFolderRequest {
    path: String,
}

#[derive(Serialize)]
struct CreateFolderResponse {
    path: String,
}

struct NotebookListing {
    files: Vec<FileEntry>,
    directories: Vec<String>,
}

fn build_state(config: ServerConfig, port: u16) -> Result<AppState, String> {
    if config.token.is_empty() {
        return Err("local UI capability must not be empty".into());
    }
    let mut quack_url = parse_loopback_url(&config.quack_url, "Quack URL")?;
    if quack_url.path().is_empty() || quack_url.path() == "/" {
        quack_url.set_path("/quack");
    } else if quack_url.path() != "/quack" {
        return Err("Quack URL must be its loopback origin or /quack endpoint".into());
    }
    let assets = if !config.local_asset_directory.is_empty() {
        let path = PathBuf::from(&config.local_asset_directory);
        if !path.is_absolute() || !path.is_dir() {
            return Err("local asset directory must be an existing absolute directory".into());
        }
        AssetSource::Local(
            path.canonicalize()
                .map_err(|error| format!("failed to resolve local asset directory: {error}"))?,
        )
    } else if !config.remote_asset_url.is_empty() {
        AssetSource::Remote(parse_remote_asset_url(&config.remote_asset_url)?)
    } else {
        return Err("remote asset URL or local asset directory is required".into());
    };
    let root = notebook_root(&config.notebook_root)?;
    let notebooks = NotebookStore::open(root)?;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(24 * 60 * 60))
        .build()
        .map_err(|error| format!("failed to build HTTP client: {error}"))?;
    Ok(AppState {
        port,
        origin: format!("http://127.0.0.1:{port}"),
        token: config.token,
        duckdb_version: config.duckdb_version,
        quack_url,
        client,
        assets,
        notebooks: Arc::new(notebooks),
    })
}

fn parse_loopback_url(value: &str, label: &str) -> Result<Url, String> {
    let url = Url::parse(value).map_err(|error| format!("invalid {label}: {error}"))?;
    if url.scheme() != "http"
        || !matches!(url.host_str(), Some("127.0.0.1") | Some("localhost"))
        || url.port_or_known_default().is_none()
    {
        return Err(format!("{label} must be a loopback HTTP URL"));
    }
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(format!(
            "{label} must not contain credentials, a query, or a fragment"
        ));
    }
    Ok(url)
}

fn parse_remote_asset_url(value: &str) -> Result<Url, String> {
    let lowered = value.to_ascii_lowercase();
    if value.contains('\\')
        || lowered.contains("%2e")
        || lowered.contains("%5c")
        || lowered.contains("/../")
        || lowered.ends_with("/..")
    {
        return Err("remote asset URL contains an invalid path".into());
    }
    let mut url =
        Url::parse(value).map_err(|error| format!("invalid remote asset URL: {error}"))?;
    let loopback = matches!(url.host_str(), Some("127.0.0.1") | Some("localhost"));
    if url.scheme() != "https" && !(url.scheme() == "http" && loopback) {
        return Err(
            "remote asset URL must use HTTPS unless it is a loopback development server".into(),
        );
    }
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err("remote asset URL must not contain credentials, a query, or a fragment".into());
    }
    if !url.path().ends_with('/') {
        let path = format!("{}/", url.path());
        url.set_path(&path);
    }
    Ok(url)
}

fn notebook_root(configured: &str) -> Result<PathBuf, String> {
    let configured = if configured.is_empty() {
        "~/Documents/sqlnb"
    } else {
        configured
    };
    let path = if configured == "~" || configured.starts_with("~/") {
        let home =
            std::env::var_os("HOME").ok_or("HOME is unavailable; notebookRoot must be absolute")?;
        let suffix = configured.strip_prefix("~/").unwrap_or("");
        PathBuf::from(home).join(suffix)
    } else {
        PathBuf::from(configured)
    };
    if !path.is_absolute() {
        return Err("notebookRoot must be absolute or start with ~/".into());
    }
    fs::create_dir_all(&path)
        .map_err(|error| format!("failed to create notebook root: {error}"))?;
    let metadata = fs::symlink_metadata(&path)
        .map_err(|error| format!("failed to inspect notebook root: {error}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err("notebookRoot must be a real directory, not a symlink".into());
    }
    path.canonicalize()
        .map_err(|error| format!("failed to resolve notebook root: {error}"))
}

impl NotebookStore {
    fn open(root: PathBuf) -> Result<Self, String> {
        let root_fd = open_directory(&root)
            .map_err(|error| format!("failed to open notebook root: {error}"))?;
        Ok(Self {
            root,
            root_fd,
            writes: Mutex::new(()),
        })
    }

    fn list(&self) -> Result<NotebookListing, ApiError> {
        let mut files = Vec::new();
        let mut directories = Vec::new();
        list_directory_fd(
            self.root_fd.as_raw_fd(),
            Path::new(""),
            &mut files,
            &mut directories,
        )?;
        files.sort_by(|left, right| left.path.cmp(&right.path));
        directories.sort();
        Ok(NotebookListing { files, directories })
    }

    fn read(&self, relative: &Path) -> Result<(String, String), ApiError> {
        let file = self.open_file(relative)?;
        let metadata = file.metadata().map_err(file_io)?;
        if !metadata.is_file() || metadata.len() > MAX_NOTEBOOK_BYTES as u64 {
            return Err(ApiError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "file_too_large",
                "notebook exceeds the 16 MiB limit",
            ));
        }
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        file.take((MAX_NOTEBOOK_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(file_io)?;
        if bytes.len() > MAX_NOTEBOOK_BYTES {
            return Err(ApiError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "file_too_large",
                "notebook exceeds the 16 MiB limit",
            ));
        }
        let revision = revision(&bytes);
        let source = String::from_utf8(bytes).map_err(|_| {
            ApiError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid_encoding",
                "notebook must be UTF-8",
            )
        })?;
        Ok((source, revision))
    }

    fn write(&self, relative: &Path, source: &[u8], expected: &str) -> Result<String, ApiError> {
        if source.len() > MAX_NOTEBOOK_BYTES {
            return Err(ApiError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "file_too_large",
                "notebook exceeds the 16 MiB limit",
            ));
        }
        let _guard = self.writes.lock().map_err(|_| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "file_io",
                "file lock is poisoned",
            )
        })?;
        let (parent, name) = self.open_parent(relative, true)?;
        let current = open_file_at(parent.as_raw_fd(), &name);
        match (expected.is_empty(), current) {
            (true, Ok(_)) => return Err(conflict("file already exists")),
            (true, Err(error)) if error.raw_os_error() == Some(libc::ENOENT) => {}
            (true, Err(error)) => return Err(file_io(error)),
            (false, Ok(file)) => {
                let metadata = file.metadata().map_err(file_io)?;
                if !metadata.is_file() || metadata.len() > MAX_NOTEBOOK_BYTES as u64 {
                    return Err(conflict("file is not a regular notebook or is too large"));
                }
                let mut bytes = Vec::new();
                file.take((MAX_NOTEBOOK_BYTES + 1) as u64)
                    .read_to_end(&mut bytes)
                    .map_err(file_io)?;
                if bytes.len() > MAX_NOTEBOOK_BYTES || revision(&bytes) != expected {
                    return Err(conflict("file changed since it was opened"));
                }
            }
            (false, Err(error)) if error.raw_os_error() == Some(libc::ENOENT) => {
                return Err(conflict("file no longer exists"))
            }
            (false, Err(error)) => return Err(file_io(error)),
        }
        atomic_write_at(parent.as_raw_fd(), &name, source, expected).map_err(|error| {
            if error.raw_os_error() == Some(libc::EEXIST) {
                conflict("file already exists")
            } else if error.raw_os_error() == Some(libc::EBUSY) {
                conflict("file changed since it was opened")
            } else {
                file_io(error)
            }
        })?;
        Ok(revision(source))
    }

    fn create_folder(&self, relative: &Path) -> Result<(), ApiError> {
        let _guard = self.writes.lock().map_err(|_| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "file_io",
                "file lock is poisoned",
            )
        })?;
        let (parent, name) = self.open_parent(relative, true)?;
        let created = unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o755) };
        if created != 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EEXIST) {
                return Err(ApiError::new(
                    StatusCode::CONFLICT,
                    "already_exists",
                    "folder already exists",
                ));
            }
            return Err(file_io(error));
        }
        if unsafe { libc::fsync(parent.as_raw_fd()) } != 0 {
            return Err(file_io(std::io::Error::last_os_error()));
        }
        Ok(())
    }

    fn move_file(
        &self,
        source: &Path,
        destination: &Path,
        expected_revision: Option<&str>,
    ) -> Result<String, ApiError> {
        let _guard = self.writes.lock().map_err(|_| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "file_io",
                "file lock is poisoned",
            )
        })?;
        let (source_parent, source_name) = self.open_parent(source, false)?;
        let source_file =
            open_file_at(source_parent.as_raw_fd(), &source_name).map_err(|error| {
                if error.raw_os_error() == Some(libc::ENOENT) {
                    ApiError::new(
                        StatusCode::NOT_FOUND,
                        "not_found",
                        "notebook does not exist",
                    )
                } else {
                    file_io(error)
                }
            })?;
        let source_metadata = source_file.metadata().map_err(file_io)?;
        if !source_metadata.is_file() || source_metadata.len() > MAX_NOTEBOOK_BYTES as u64 {
            return Err(ApiError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "file_too_large",
                "notebook must be a regular file within the 16 MiB limit",
            ));
        }
        let mut bytes = Vec::with_capacity(source_metadata.len() as usize);
        source_file
            .take((MAX_NOTEBOOK_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(file_io)?;
        if bytes.len() > MAX_NOTEBOOK_BYTES {
            return Err(ApiError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "file_too_large",
                "notebook exceeds the 16 MiB limit",
            ));
        }
        let source_revision = revision(&bytes);
        if expected_revision.is_some_and(|expected| expected != source_revision) {
            return Err(conflict("file changed since it was opened"));
        }
        if source == destination {
            return Ok(source_revision);
        }

        // Opening with create=false deliberately requires every destination
        // parent to exist. The move endpoint never creates directory state.
        let (destination_parent, destination_name) = self.open_parent(destination, false)?;
        let current_metadata =
            metadata_at(source_parent.as_raw_fd(), &source_name).map_err(file_io)?;
        if current_metadata.st_dev as u64 != source_metadata.dev()
            || current_metadata.st_ino as u64 != source_metadata.ino()
            || (current_metadata.st_mode & libc::S_IFMT) != libc::S_IFREG
        {
            return Err(conflict("file changed while it was being moved"));
        }
        rename_no_replace(
            source_parent.as_raw_fd(),
            &source_name,
            destination_parent.as_raw_fd(),
            &destination_name,
        )
        .map_err(|error| match error.raw_os_error() {
            Some(libc::EEXIST) | Some(libc::ENOTEMPTY) => ApiError::new(
                StatusCode::CONFLICT,
                "already_exists",
                "destination already exists",
            ),
            Some(libc::ENOENT) => conflict("file changed while it was being moved"),
            _ => file_io(error),
        })?;
        // The rename is already committed at this point. Directory fsync is
        // best-effort: reporting an error would make the client believe the
        // source path still exists and leave its active document stale.
        let _ = unsafe { libc::fsync(source_parent.as_raw_fd()) };
        let _ = unsafe { libc::fsync(destination_parent.as_raw_fd()) };
        Ok(source_revision)
    }

    fn open_file(&self, relative: &Path) -> Result<File, ApiError> {
        let (parent, name) = self.open_parent(relative, false)?;
        open_file_at(parent.as_raw_fd(), &name).map_err(|error| {
            if error.raw_os_error() == Some(libc::ENOENT) {
                ApiError::new(
                    StatusCode::NOT_FOUND,
                    "not_found",
                    "notebook does not exist",
                )
            } else {
                file_io(error)
            }
        })
    }

    fn open_parent(&self, relative: &Path, create: bool) -> Result<(OwnedFd, CString), ApiError> {
        let mut components: Vec<_> = relative.components().collect();
        let name = match components.pop() {
            Some(Component::Normal(name)) => cstring_component(name)?,
            _ => return Err(invalid_path()),
        };
        let duplicated = unsafe { libc::dup(self.root_fd.as_raw_fd()) };
        if duplicated < 0 {
            return Err(file_io(std::io::Error::last_os_error()));
        }
        let mut directory = unsafe { OwnedFd::from_raw_fd(duplicated) };
        for component in components {
            let Component::Normal(name) = component else {
                return Err(invalid_path());
            };
            let name = cstring_component(name)?;
            match open_directory_at(directory.as_raw_fd(), &name) {
                Ok(next) => directory = next,
                Err(error) if create && error.raw_os_error() == Some(libc::ENOENT) => {
                    mkdir_at(directory.as_raw_fd(), &name).map_err(file_io)?;
                    directory = open_directory_at(directory.as_raw_fd(), &name).map_err(file_io)?;
                }
                Err(error) => return Err(file_io(error)),
            }
        }
        Ok((directory, name))
    }
}

fn validate_relative_path(value: &str) -> Result<PathBuf, ApiError> {
    let path = validate_relative_components(value)?;
    if !allowed_extension(&path) {
        return Err(invalid_path());
    }
    Ok(path)
}

fn validate_folder_path(value: &str) -> Result<PathBuf, ApiError> {
    validate_relative_components(value).map_err(|_| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_path",
            "path must be a relative folder path without traversal",
        )
    })
}

fn validate_relative_components(value: &str) -> Result<PathBuf, ApiError> {
    if value.is_empty() || value.contains('\\') || value.contains('\0') {
        return Err(invalid_path());
    }
    let path = PathBuf::from(value);
    if path.is_absolute()
        || !path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
    {
        return Err(invalid_path());
    }
    Ok(path)
}

fn allowed_extension(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|value| value.to_str())
            .map(|value| value.to_ascii_lowercase())
            .as_deref(),
        Some("sql") | Some("sqlnb")
    )
}

fn invalid_path() -> ApiError {
    ApiError::new(
        StatusCode::BAD_REQUEST,
        "invalid_path",
        "path must be a relative .sql or .sqlnb path without traversal",
    )
}

fn conflict(message: &str) -> ApiError {
    ApiError::new(StatusCode::CONFLICT, "revision_conflict", message)
}

fn file_io(error: std::io::Error) -> ApiError {
    ApiError::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        "file_io",
        error.to_string(),
    )
}

fn revision(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn slash_path(path: &Path) -> Result<String, ApiError> {
    let parts: Result<Vec<_>, _> = path
        .components()
        .map(|component| match component {
            Component::Normal(value) => value
                .to_str()
                .map(ToOwned::to_owned)
                .ok_or_else(invalid_path),
            _ => Err(invalid_path()),
        })
        .collect();
    Ok(parts?.join("/"))
}

fn cstring_component(value: &std::ffi::OsStr) -> Result<CString, ApiError> {
    use std::os::unix::ffi::OsStrExt;
    CString::new(value.as_bytes()).map_err(|_| invalid_path())
}

struct DirectoryStream(*mut libc::DIR);

impl Drop for DirectoryStream {
    fn drop(&mut self) {
        unsafe {
            libc::closedir(self.0);
        }
    }
}

fn list_directory_fd(
    directory: libc::c_int,
    relative: &Path,
    files: &mut Vec<FileEntry>,
    directories: &mut Vec<String>,
) -> Result<(), ApiError> {
    let dot = c".";
    // openat(".") creates an independent open file description. dup would
    // share the directory offset and make a second listing appear empty.
    let reopened = open_directory_at(directory, dot).map_err(file_io)?;
    let reopened = reopened.into_raw_fd();
    let stream = unsafe { libc::fdopendir(reopened) };
    if stream.is_null() {
        unsafe { libc::close(reopened) };
        return Err(file_io(std::io::Error::last_os_error()));
    }
    let stream = DirectoryStream(stream);
    loop {
        clear_errno();
        let entry = unsafe { libc::readdir(stream.0) };
        if entry.is_null() {
            let error = current_errno();
            if error != 0 {
                return Err(file_io(std::io::Error::from_raw_os_error(error)));
            }
            break;
        }
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
        if name.to_bytes() == b"." || name.to_bytes() == b".." {
            continue;
        }
        let mut metadata: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe {
            libc::fstatat(
                directory,
                name.as_ptr(),
                &mut metadata,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } != 0
        {
            return Err(file_io(std::io::Error::last_os_error()));
        }
        let kind = metadata.st_mode & libc::S_IFMT;
        if kind == libc::S_IFLNK {
            continue;
        }
        let file_name = std::ffi::OsString::from_vec(name.to_bytes().to_vec());
        let child_relative = relative.join(file_name);
        if kind == libc::S_IFDIR {
            let child_name = CString::new(name.to_bytes()).map_err(|_| invalid_path())?;
            let child = open_directory_at(directory, &child_name).map_err(file_io)?;
            if let Ok(path) = slash_path(&child_relative) {
                directories.push(path);
            }
            list_directory_fd(child.as_raw_fd(), &child_relative, files, directories)?;
        } else if kind == libc::S_IFREG && allowed_extension(&child_relative) {
            if let Ok(path) = slash_path(&child_relative) {
                files.push(FileEntry { path });
            }
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn clear_errno() {
    unsafe { *libc::__error() = 0 }
}

#[cfg(target_os = "macos")]
fn current_errno() -> i32 {
    unsafe { *libc::__error() }
}

#[cfg(target_os = "linux")]
fn clear_errno() {
    unsafe { *libc::__errno_location() = 0 }
}

#[cfg(target_os = "linux")]
fn current_errno() -> i32 {
    unsafe { *libc::__errno_location() }
}

fn open_directory(path: &Path) -> std::io::Result<OwnedFd> {
    use std::os::unix::ffi::OsStrExt;
    let path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::from_raw_os_error(libc::EINVAL))?;
    let fd = unsafe {
        libc::open(
            path.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }
}

fn open_directory_at(parent: libc::c_int, name: &CStr) -> std::io::Result<OwnedFd> {
    let fd = unsafe {
        libc::openat(
            parent,
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }
}

fn mkdir_at(parent: libc::c_int, name: &CStr) -> std::io::Result<()> {
    let result = unsafe { libc::mkdirat(parent, name.as_ptr(), 0o755) };
    if result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EEXIST) {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

fn open_file_at(parent: libc::c_int, name: &CStr) -> std::io::Result<File> {
    let fd = unsafe {
        libc::openat(
            parent,
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

fn metadata_at(parent: libc::c_int, name: &CStr) -> std::io::Result<libc::stat> {
    let mut metadata: libc::stat = unsafe { std::mem::zeroed() };
    let result = unsafe {
        libc::fstatat(
            parent,
            name.as_ptr(),
            &mut metadata,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if result == 0 {
        Ok(metadata)
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(target_os = "macos")]
fn rename_no_replace(
    source_parent: libc::c_int,
    source_name: &CStr,
    destination_parent: libc::c_int,
    destination_name: &CStr,
) -> std::io::Result<()> {
    let result = unsafe {
        libc::renameatx_np(
            source_parent,
            source_name.as_ptr(),
            destination_parent,
            destination_name.as_ptr(),
            libc::RENAME_EXCL,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(target_os = "linux")]
fn rename_no_replace(
    source_parent: libc::c_int,
    source_name: &CStr,
    destination_parent: libc::c_int,
    destination_name: &CStr,
) -> std::io::Result<()> {
    const RENAME_NOREPLACE: libc::c_uint = 1;
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            source_parent,
            source_name.as_ptr(),
            destination_parent,
            destination_name.as_ptr(),
            RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

fn atomic_write_at(
    parent: libc::c_int,
    name: &CStr,
    source: &[u8],
    expected_revision: &str,
) -> std::io::Result<()> {
    let temporary = CString::new(format!(".sidequery-{}.tmp", uuid::Uuid::new_v4()))
        .expect("UUID contains no NUL");
    let fd = unsafe {
        libc::openat(
            parent,
            temporary.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut file = unsafe { File::from_raw_fd(fd) };
    let result = (|| {
        file.write_all(source)?;
        file.sync_all()?;
        if expected_revision.is_empty() {
            let linked =
                unsafe { libc::linkat(parent, temporary.as_ptr(), parent, name.as_ptr(), 0) };
            if linked != 0 {
                return Err(std::io::Error::last_os_error());
            }
            let _ = unsafe { libc::unlinkat(parent, temporary.as_ptr(), 0) };
        } else {
            // Recheck after the temporary file is durable, immediately before
            // replacement. Portable filesystems do not expose a cross-process
            // compare-and-swap rename, so a non-cooperating writer can still
            // race the final rename after this check.
            let current = open_file_at(parent, name)?;
            let metadata = current.metadata()?;
            if !metadata.is_file() || metadata.len() > MAX_NOTEBOOK_BYTES as u64 {
                return Err(std::io::Error::from_raw_os_error(libc::EBUSY));
            }
            let mut bytes = Vec::with_capacity(metadata.len() as usize);
            current
                .take((MAX_NOTEBOOK_BYTES + 1) as u64)
                .read_to_end(&mut bytes)?;
            if bytes.len() > MAX_NOTEBOOK_BYTES || revision(&bytes) != expected_revision {
                return Err(std::io::Error::from_raw_os_error(libc::EBUSY));
            }
            let renamed =
                unsafe { libc::renameat(parent, temporary.as_ptr(), parent, name.as_ptr()) };
            if renamed != 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
        let synced = unsafe { libc::fsync(parent) };
        if synced != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = unsafe { libc::unlinkat(parent, temporary.as_ptr(), 0) };
    }
    result
}

async fn security_middleware(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    let expected_127 = format!("127.0.0.1:{}", state.port);
    let expected_localhost = format!("localhost:{}", state.port);
    let valid = request
        .headers()
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|host| host == expected_127 || host == expected_localhost);
    if !valid {
        return ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_host",
            "invalid local UI host",
        )
        .into_response();
    }
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CSP),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    response
}

fn authenticate(headers: &HeaderMap, state: &AppState) -> Result<(), ApiError> {
    let authorization = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok());
    let supplied = authorization
        .and_then(|value| value.strip_prefix("Bearer "))
        .unwrap_or("");
    let token_matches = supplied.len() == state.token.len()
        && bool::from(supplied.as_bytes().ct_eq(state.token.as_bytes()));
    let origin = headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok());
    if !token_matches || origin.is_some_and(|value| value != state.origin) {
        return Err(ApiError::new(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "invalid local UI capability or origin",
        ));
    }
    Ok(())
}

fn validate_origin(headers: &HeaderMap, state: &AppState) -> Result<(), ApiError> {
    let origin = headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok());
    if origin.is_some_and(|value| value != state.origin) {
        return Err(ApiError::new(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "invalid local Quack origin",
        ));
    }
    Ok(())
}

async fn bootstrap(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    authenticate(&headers, &state)?;
    Ok((
        [
            (header::ACCESS_CONTROL_ALLOW_ORIGIN, state.origin.clone()),
            (header::VARY, "Origin".into()),
        ],
        Json(BootstrapResponse {
            bridge_version: 1,
            protocol: "quack",
            endpoint: "/quack",
            token: state.token.clone(),
            supports_cancel: false,
            engine: "native-duckdb",
            duckdb_version: state.duckdb_version.clone(),
        }),
    ))
}

async fn list_files(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    authenticate(&headers, &state)?;
    let notebooks = state.notebooks.clone();
    let listing = tokio::task::spawn_blocking(move || notebooks.list())
        .await
        .map_err(|_| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "file_io",
                "file task failed",
            )
        })??;
    Ok(Json(FilesResponse {
        root: state.notebooks.root.display().to_string(),
        files: listing.files,
        directories: listing.directories,
    }))
}

async fn create_folder(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<CreateFolderRequest>,
) -> Result<impl IntoResponse, ApiError> {
    authenticate(&headers, &state)?;
    let path = validate_folder_path(&body.path)?;
    let notebooks = state.notebooks.clone();
    let path_for_create = path.clone();
    tokio::task::spawn_blocking(move || notebooks.create_folder(&path_for_create))
        .await
        .map_err(|_| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "file_io",
                "file task failed",
            )
        })??;
    Ok(Json(CreateFolderResponse {
        path: slash_path(&path)?,
    }))
}

async fn get_file(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<FileQuery>,
) -> Result<impl IntoResponse, ApiError> {
    authenticate(&headers, &state)?;
    let path = validate_relative_path(&query.path)?;
    let notebooks = state.notebooks.clone();
    let path_for_read = path.clone();
    let (source, revision) = tokio::task::spawn_blocking(move || notebooks.read(&path_for_read))
        .await
        .map_err(|_| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "file_io",
                "file task failed",
            )
        })??;
    Ok(Json(FileResponse {
        path: slash_path(&path)?,
        source,
        revision,
    }))
}

async fn put_file(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<PutFileRequest>,
) -> Result<impl IntoResponse, ApiError> {
    authenticate(&headers, &state)?;
    let path = validate_relative_path(&body.path)?;
    let notebooks = state.notebooks.clone();
    let path_for_write = path.clone();
    let revision = tokio::task::spawn_blocking(move || {
        notebooks.write(
            &path_for_write,
            body.source.as_bytes(),
            &body.expected_revision,
        )
    })
    .await
    .map_err(|_| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "file_io",
            "file task failed",
        )
    })??;
    Ok(Json(PutFileResponse {
        path: slash_path(&path)?,
        revision,
    }))
}

async fn move_file(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<MoveFileRequest>,
) -> Result<impl IntoResponse, ApiError> {
    authenticate(&headers, &state)?;
    let source = validate_relative_path(&body.path)?;
    let destination = validate_relative_path(&body.destination_path)?;
    let response_path = slash_path(&destination)?;
    let notebooks = state.notebooks.clone();
    let revision = tokio::task::spawn_blocking(move || {
        notebooks.move_file(&source, &destination, body.expected_revision.as_deref())
    })
    .await
    .map_err(|_| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "file_io",
            "file task failed",
        )
    })??;
    Ok(Json(MoveFileResponse {
        path: response_path,
        revision,
    }))
}

async fn quack(
    State(state): State<AppState>,
    headers: HeaderMap,
    request: Request,
) -> Result<Response, ApiError> {
    validate_origin(&headers, &state)?;
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("application/vnd.duckdb");
    let body = to_bytes(request.into_body(), MAX_QUACK_MESSAGE_BYTES)
        .await
        .map_err(|_| {
            ApiError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "quack_message_too_large",
                "Quack message exceeds 64 MiB",
            )
        })?;
    let upstream = state
        .client
        .post(state.quack_url.clone())
        .header(header::CONTENT_TYPE, content_type)
        .body(body)
        .send()
        .await
        .map_err(|_| {
            ApiError::new(
                StatusCode::BAD_GATEWAY,
                "quack_unavailable",
                "DuckDB Quack endpoint is unavailable",
            )
        })?;
    let status = upstream.status();
    let bytes = upstream.bytes().await.map_err(|_| {
        ApiError::new(
            StatusCode::BAD_GATEWAY,
            "quack_unavailable",
            "DuckDB Quack endpoint is unavailable",
        )
    })?;
    Ok((
        status,
        [
            (header::CONTENT_TYPE, "application/vnd.duckdb"),
            (header::ACCESS_CONTROL_ALLOW_ORIGIN, state.origin.as_str()),
            (header::VARY, "Origin"),
        ],
        bytes,
    )
        .into_response())
}

async fn options_quack(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    validate_origin(&headers, &state)?;
    Ok((
        StatusCode::NO_CONTENT,
        [
            (header::ACCESS_CONTROL_ALLOW_ORIGIN, state.origin),
            (header::ACCESS_CONTROL_ALLOW_METHODS, "POST, OPTIONS".into()),
            (header::ACCESS_CONTROL_ALLOW_HEADERS, "*".into()),
        ],
    )
        .into_response())
}

async fn static_asset(
    State(state): State<AppState>,
    request: Request,
) -> Result<Response, ApiError> {
    let path = request.uri().path().trim_start_matches('/');
    if path.contains("..")
        || path.contains('\\')
        || path.to_ascii_lowercase().contains("%2e")
        || path.to_ascii_lowercase().contains("%5c")
    {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_asset_path",
            "invalid asset path",
        ));
    }
    let path = if path.is_empty() { "index.html" } else { path };
    match &state.assets {
        AssetSource::Local(root) => {
            let file = root.join(path);
            let canonical = file.canonicalize().map_err(|_| {
                ApiError::new(StatusCode::NOT_FOUND, "not_found", "asset does not exist")
            })?;
            if !canonical.starts_with(root) || !canonical.is_file() {
                return Err(ApiError::new(
                    StatusCode::NOT_FOUND,
                    "not_found",
                    "asset does not exist",
                ));
            }
            let bytes = tokio::fs::read(&canonical).await.map_err(|_| {
                ApiError::new(StatusCode::NOT_FOUND, "not_found", "asset does not exist")
            })?;
            let mime = mime_guess::from_path(&canonical)
                .first_or_octet_stream()
                .to_string();
            Ok((StatusCode::OK, [(header::CONTENT_TYPE, mime)], bytes).into_response())
        }
        AssetSource::Remote(base) => {
            let url = base.join(path).map_err(|_| {
                ApiError::new(
                    StatusCode::BAD_REQUEST,
                    "invalid_asset_path",
                    "invalid asset path",
                )
            })?;
            let upstream = state
                .client
                .get(url)
                .timeout(Duration::from_secs(30))
                .send()
                .await
                .map_err(|_| {
                    ApiError::new(
                        StatusCode::BAD_GATEWAY,
                        "asset_unavailable",
                        "remote UI asset is unavailable",
                    )
                })?;
            let status = upstream.status();
            let content_type = upstream
                .headers()
                .get(header::CONTENT_TYPE)
                .cloned()
                .unwrap_or_else(|| HeaderValue::from_static("application/octet-stream"));
            let bytes = upstream.bytes().await.map_err(|_| {
                ApiError::new(
                    StatusCode::BAD_GATEWAY,
                    "asset_unavailable",
                    "remote UI asset is unavailable",
                )
            })?;
            Ok((status, [(header::CONTENT_TYPE, content_type)], bytes).into_response())
        }
    }
}

fn router(state: AppState) -> Router {
    Router::new()
        .route("/api/v1/bootstrap", get(bootstrap))
        .route("/api/v1/files", get(list_files))
        .route("/api/v1/file", get(get_file).put(put_file))
        .route("/api/v1/file/move", post(move_file))
        .route("/api/v1/folder", post(create_folder))
        .route("/quack", post(quack).options(options_quack))
        .fallback(get(static_asset))
        .layer(DefaultBodyLimit::max(MAX_QUACK_MESSAGE_BYTES))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            security_middleware,
        ))
        .with_state(state)
}

fn start(config: ServerConfig) -> Result<ServerHandle, String> {
    let address = SocketAddrV4::new(Ipv4Addr::LOCALHOST, config.port);
    let listener = TcpListener::bind(address)
        .map_err(|error| format!("failed to bind Sidequery UI to 127.0.0.1: {error}"))?;
    listener
        .set_nonblocking(true)
        .map_err(|error| format!("failed to configure Sidequery UI listener: {error}"))?;
    let port = listener
        .local_addr()
        .map_err(|error| format!("failed to inspect Sidequery UI listener: {error}"))?
        .port();
    let state = build_state(config, port)?;
    let app = router(state);
    let (shutdown, shutdown_rx) = watch::channel(false);
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let thread = thread::Builder::new()
        .name("sidequery-ui-http".into())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build();
            let runtime = match runtime {
                Ok(runtime) => runtime,
                Err(error) => {
                    let _ = ready_tx.send(Err(format!("failed to create HTTP runtime: {error}")));
                    return;
                }
            };
            runtime.block_on(async move {
                let listener = match tokio::net::TcpListener::from_std(listener) {
                    Ok(listener) => listener,
                    Err(error) => {
                        let _ =
                            ready_tx.send(Err(format!("failed to adopt HTTP listener: {error}")));
                        return;
                    }
                };
                let _ = ready_tx.send(Ok(()));
                let mut graceful_rx = shutdown_rx;
                let wait_for_shutdown = async move {
                    while !*graceful_rx.borrow() && graceful_rx.changed().await.is_ok() {}
                };
                let _ = axum::serve(listener, app)
                    .with_graceful_shutdown(wait_for_shutdown)
                    .await;
            });
        })
        .map_err(|error| format!("failed to start Sidequery UI server thread: {error}"))?;
    match ready_rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            let _ = thread.join();
            return Err(error);
        }
        Err(_) => {
            let _ = shutdown.send(true);
            let _ = thread.join();
            return Err("Sidequery UI server did not become ready".into());
        }
    }
    Ok(ServerHandle {
        port,
        shutdown: Some(shutdown),
        thread: Some(thread),
    })
}

fn set_error(error_out: *mut *mut c_char, message: String) {
    if error_out.is_null() {
        return;
    }
    let sanitized = message.replace('\0', " ");
    let value = CString::new(sanitized).expect("sanitized string contains no NUL");
    unsafe {
        *error_out = value.into_raw();
    }
}

#[no_mangle]
pub extern "C" fn sidequery_http_start(
    config_json: *const c_char,
    error_out: *mut *mut c_char,
) -> *mut c_void {
    let result = std::panic::catch_unwind(|| -> Result<ServerHandle, String> {
        if config_json.is_null() {
            return Err("config_json must not be null".into());
        }
        let config = unsafe { CStr::from_ptr(config_json) }
            .to_str()
            .map_err(|_| "config_json must be UTF-8".to_string())?;
        let config: ServerConfig = serde_json::from_str(config)
            .map_err(|error| format!("invalid server config: {error}"))?;
        start(config)
    });
    match result {
        Ok(Ok(handle)) => Box::into_raw(Box::new(handle)).cast(),
        Ok(Err(error)) => {
            set_error(error_out, error);
            ptr::null_mut()
        }
        Err(_) => {
            set_error(
                error_out,
                "unexpected panic while starting Sidequery UI server".into(),
            );
            ptr::null_mut()
        }
    }
}

#[no_mangle]
pub extern "C" fn sidequery_http_port(handle: *mut c_void) -> u16 {
    std::panic::catch_unwind(|| {
        if handle.is_null() {
            0
        } else {
            unsafe { &*(handle.cast::<ServerHandle>()) }.port
        }
    })
    .unwrap_or(0)
}

#[no_mangle]
pub extern "C" fn sidequery_http_stop(handle: *mut c_void) {
    let _ = std::panic::catch_unwind(|| {
        if !handle.is_null() {
            let mut handle = unsafe { Box::from_raw(handle.cast::<ServerHandle>()) };
            handle.stop();
        }
    });
}

#[no_mangle]
pub extern "C" fn sidequery_http_string_free(value: *mut c_char) {
    let _ = std::panic::catch_unwind(|| {
        if !value.is_null() {
            drop(unsafe { CString::from_raw(value) });
        }
    });
}

#[no_mangle]
pub extern "C" fn sidequery_http_unused_port() -> u16 {
    std::panic::catch_unwind(|| {
        TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .and_then(|listener| listener.local_addr())
            .map(|address| address.port())
            .unwrap_or(0)
    })
    .unwrap_or(0)
}

#[cfg(test)]
mod tests;
