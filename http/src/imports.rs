//! Data files uploaded for import into DuckDB.
//!
//! A browser does not tell a page where a dropped file is stored, so the page
//! sends the file's bytes here and imports the saved copy by its absolute path.
//! Uploads go only into one folder of the notebook root, under a sanitized name
//! with a data-file extension, never replace an existing file, and are bounded
//! while they stream. They stay until the user deletes them.
use crate::*;
use axum::{body::Body, routing::MethodRouter};
use bytes::Bytes;
use http_body_util::BodyExt;
use std::ffi::OsStr;

/// Folder of the notebook root that holds uploaded files. It is left out of the
/// notebook listing and ignored by Git.
pub(crate) const IMPORTS_FOLDER: &str = ".sqlnb-imports";
/// Largest upload accepted, advertised in the bootstrap response.
pub(crate) const MAX_IMPORT_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_NAME_CHARS: usize = 120;
const MAX_NUMBERED_NAMES: u32 = 10_000;
const DATA_EXTENSIONS: [&str; 7] = ["csv", "tsv", "txt", "parquet", "json", "jsonl", "ndjson"];
const COMPRESSED_EXTENSIONS: [&str; 2] = ["gz", "zst"];

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Capability {
    endpoint: &'static str,
    max_bytes: u64,
    folder: String,
}

pub(crate) fn capability(state: &AppState) -> Capability {
    Capability {
        endpoint: "/api/v1/import",
        max_bytes: MAX_IMPORT_BYTES,
        folder: state
            .notebooks
            .root
            .join(IMPORTS_FOLDER)
            .display()
            .to_string(),
    }
}

/// `PUT` saves the request body as a new file; `DELETE` removes one saved file.
/// The handler enforces its own streaming limit instead of the router default.
pub(crate) fn routes() -> MethodRouter<AppState> {
    axum::routing::put(upload)
        .delete(remove)
        .layer(DefaultBodyLimit::disable())
}

/// Whether a listing entry lies in the imports folder.
pub(crate) fn is_imports_entry(path: &str) -> bool {
    path == IMPORTS_FOLDER
        || path
            .strip_prefix(IMPORTS_FOLDER)
            .is_some_and(|rest| rest.starts_with('/'))
}

#[derive(Deserialize)]
struct UploadQuery {
    name: String,
    /// Client-chosen UUID by which the client can cancel this upload.
    id: Option<String>,
}

#[derive(Deserialize)]
struct RemoveQuery {
    name: Option<String>,
    id: Option<String>,
}

/// Uploads a client may still cancel, by notebook root and upload id. Cancelling
/// by id covers the race an aborted request cannot: the server may have received
/// the whole body, and saved it, before the client's abort arrives.
#[derive(Default)]
struct Uploads {
    states: std::collections::HashMap<(PathBuf, String), UploadState>,
    order: std::collections::VecDeque<(PathBuf, String)>,
}

enum UploadState {
    Receiving,
    Cancelled,
    Saved(String),
}

const MAX_TRACKED_UPLOADS: usize = 1024;
static UPLOADS: std::sync::LazyLock<Mutex<Uploads>> = std::sync::LazyLock::new(Default::default);

fn uploads() -> std::sync::MutexGuard<'static, Uploads> {
    UPLOADS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Uploads {
    fn set(&mut self, key: &(PathBuf, String), state: UploadState) {
        if self.states.insert(key.clone(), state).is_none() {
            self.order.push_back(key.clone());
        }
        while self.order.len() > MAX_TRACKED_UPLOADS {
            if let Some(oldest) = self.order.pop_front() {
                self.states.remove(&oldest);
            }
        }
    }
}

fn upload_id(value: &str) -> Result<String, ApiError> {
    match uuid::Uuid::parse_str(value) {
        Ok(id) if id.hyphenated().to_string() == value => Ok(value.to_owned()),
        _ => Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_upload_id",
            "id must be a lowercase hyphenated UUID",
        )),
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct UploadResponse {
    pub(crate) path: String,
    pub(crate) name: String,
    pub(crate) bytes: u64,
}

fn invalid_name() -> ApiError {
    ApiError::new(
        StatusCode::BAD_REQUEST,
        "invalid_name",
        "name must be a file name without path separators",
    )
}

fn unsupported_type() -> ApiError {
    ApiError::new(
        StatusCode::BAD_REQUEST,
        "unsupported_file_type",
        "only .csv, .tsv, .txt, .parquet, .json, .jsonl and .ndjson files, optionally .gz or .zst compressed, can be imported",
    )
}

fn too_large(limit: u64) -> ApiError {
    ApiError::new(
        StatusCode::PAYLOAD_TOO_LARGE,
        "file_too_large",
        format!("uploads are limited to {limit} bytes"),
    )
}

fn aborted() -> ApiError {
    ApiError::new(
        StatusCode::BAD_REQUEST,
        "upload_aborted",
        "the upload did not complete",
    )
}

fn task_failed() -> ApiError {
    file_io(std::io::Error::other("upload task failed"))
}

/// Splits a lower-case data extension, with an optional compression suffix, off a name.
fn split_data_extension(name: &str) -> Option<(&str, String)> {
    let lower = name.to_ascii_lowercase();
    let mut stem_length = name.len();
    let mut extension = String::new();
    for compressed in COMPRESSED_EXTENSIONS {
        if let Some(rest) = lower.strip_suffix(&format!(".{compressed}")) {
            stem_length = rest.len();
            extension = format!(".{compressed}");
            break;
        }
    }
    let base = &lower[..stem_length];
    let data = DATA_EXTENSIONS
        .iter()
        .find(|data| base.ends_with(&format!(".{data}")))?;
    stem_length -= data.len() + 1;
    Some((&name[..stem_length], format!(".{data}{extension}")))
}

/// The name a supplied file name is saved under: a single component of letters,
/// digits, spaces and `-_.()`, other characters replaced, no leading dots, with an
/// allowed data extension. Names with separators or control characters are refused.
pub(crate) fn safe_name(supplied: &str) -> Result<String, ApiError> {
    if supplied.is_empty()
        || supplied
            .chars()
            .any(|c| c == '/' || c == '\\' || c.is_control())
    {
        return Err(invalid_name());
    }
    let mapped: String = supplied
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || " -_.()".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect();
    let trimmed = mapped
        .trim_start_matches(['.', ' '])
        .trim_end_matches(['.', ' ']);
    let (stem, extension) = split_data_extension(trimmed).ok_or_else(unsupported_type)?;
    let stem: String = stem.chars().take(MAX_NAME_CHARS).collect();
    let stem = stem.trim_end_matches(['.', ' ']);
    if stem.is_empty() {
        return Err(invalid_name());
    }
    let name = format!("{stem}{extension}");
    if !valid_component(OsStr::new(&name)) {
        return Err(invalid_name());
    }
    Ok(name)
}

/// `name`, then `stem (2).ext`, `stem (3).ext`, ...
fn numbered(name: &str, number: u32) -> String {
    if number == 1 {
        return name.to_owned();
    }
    let (stem, extension) = split_data_extension(name).expect("safe names keep their extension");
    format!("{stem} ({number}){extension}")
}

fn open_imports_folder(store: &NotebookStore, create: bool) -> Result<File, ApiError> {
    let name = CString::new(IMPORTS_FOLDER).expect("constant has no NUL");
    let root = store.root_fd.platform_handle();
    if create {
        mkdir_at(root, &name).map_err(file_io)?;
    }
    let folder = open_directory_at(root, &name).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            ApiError::new(StatusCode::NOT_FOUND, "not_found", "no uploaded files")
        } else {
            ApiError::new(
                StatusCode::CONFLICT,
                "invalid_imports_folder",
                format!("{IMPORTS_FOLDER} must be a real folder of the notebook root"),
            )
        }
    })?;
    if create {
        // A Git-backed notebook root must not pick up uploaded data.
        match create_new_file_at(folder.platform_handle(), c".gitignore") {
            Ok(mut ignore) => ignore.write_all(b"*\n").map_err(file_io)?,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(file_io(error)),
        }
    }
    Ok(folder)
}

enum Chunk {
    Data(Bytes),
    Finish,
}

/// Writes chunks to a temporary file and gives it the first free numbered name
/// once `Finish` arrives. If the sender goes away first, the upload was cancelled
/// by id, or anything fails, the temporary file is removed.
fn write_upload(
    store: &NotebookStore,
    name: &str,
    tracked: Option<&(PathBuf, String)>,
    mut chunks: tokio::sync::mpsc::Receiver<Chunk>,
) -> Result<(String, u64), ApiError> {
    let folder = open_imports_folder(store, true)?;
    let parent = folder.platform_handle();
    let temporary = CString::new(format!(".upload-{}.part", uuid::Uuid::new_v4()))
        .expect("UUID contains no NUL");
    let mut file = create_new_file_at(parent, &temporary).map_err(file_io)?;
    let mut written = 0u64;
    let result = (|| {
        loop {
            match chunks.blocking_recv() {
                Some(Chunk::Data(data)) => {
                    file.write_all(&data).map_err(file_io)?;
                    written += data.len() as u64;
                }
                Some(Chunk::Finish) => break,
                None => return Err(aborted()),
            }
        }
        file.sync_all().map_err(file_io)?;
        // Publishing and cancelling by id are decided under one lock: either the
        // cancel comes first and nothing is published, or it finds the saved name.
        let mut registry = tracked.map(|key| (key, uploads()));
        if let Some((key, registry)) = &registry {
            if matches!(registry.states.get(*key), Some(UploadState::Cancelled)) {
                return Err(aborted());
            }
        }
        for number in 1..=MAX_NUMBERED_NAMES {
            let candidate = numbered(name, number);
            let destination = CString::new(candidate.as_str()).map_err(|_| invalid_name())?;
            match rename_no_replace(parent, &temporary, parent, &destination) {
                Ok(()) => {
                    if let Some((key, registry)) = registry.as_mut() {
                        registry.set(key, UploadState::Saved(candidate.clone()));
                    }
                    sync_directory(parent).map_err(file_io)?;
                    return Ok(candidate);
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(file_io(error)),
            }
        }
        Err(ApiError::new(
            StatusCode::CONFLICT,
            "name_unavailable",
            "too many uploaded files share this name",
        ))
    })();
    drop(file);
    match result {
        Ok(saved) => Ok((saved, written)),
        Err(error) => {
            let _ = remove_file_at(parent, &temporary);
            Err(error)
        }
    }
}

/// Streams `body` into the imports folder, refusing it once it passes `limit` bytes.
pub(crate) async fn receive(
    store: Arc<NotebookStore>,
    name: String,
    id: Option<String>,
    mut body: Body,
    limit: u64,
) -> Result<UploadResponse, ApiError> {
    let tracked = id.map(|id| (store.root.clone(), id));
    if let Some(key) = &tracked {
        let mut registry = uploads();
        // An id is used once; one cancelled before its upload began stays cancelled.
        if registry.states.contains_key(key) {
            return Err(aborted());
        }
        registry.set(key, UploadState::Receiving);
    }
    let (sender, receiver) = tokio::sync::mpsc::channel(4);
    let writer_store = store.clone();
    let writer = tokio::task::spawn_blocking(move || {
        write_upload(&writer_store, &name, tracked.as_ref(), receiver)
    });
    let mut received = 0u64;
    let streamed = async {
        while let Some(frame) = body.frame().await {
            let frame = frame.map_err(|_| aborted())?;
            let Ok(data) = frame.into_data() else {
                continue;
            };
            received += data.len() as u64;
            if received > limit {
                return Err(too_large(limit));
            }
            if sender.send(Chunk::Data(data)).await.is_err() {
                // The writer failed; its error is the response.
                return Ok(());
            }
        }
        let _ = sender.send(Chunk::Finish).await;
        Ok(())
    }
    .await;
    drop(sender);
    // Wait for the writer even when streaming failed, so the partial file is gone
    // before the response is sent.
    let written = writer.await.map_err(|_| task_failed())?;
    streamed?;
    let (saved, bytes) = written?;
    Ok(UploadResponse {
        path: store
            .root
            .join(IMPORTS_FOLDER)
            .join(&saved)
            .display()
            .to_string(),
        name: saved,
        bytes,
    })
}

async fn upload(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<UploadQuery>,
    body: Body,
) -> Result<impl IntoResponse, ApiError> {
    authenticate(&headers, &state)?;
    let name = safe_name(&query.name)?;
    let id = query.id.as_deref().map(upload_id).transpose()?;
    let declared = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    if declared.is_some_and(|length| length > MAX_IMPORT_BYTES) {
        return Err(too_large(MAX_IMPORT_BYTES));
    }
    let response = receive(state.notebooks.clone(), name, id, body, MAX_IMPORT_BYTES).await?;
    Ok((StatusCode::CREATED, Json(response)))
}

fn delete_saved(store: &NotebookStore, name: &str) -> Result<(), ApiError> {
    let folder = open_imports_folder(store, false)?;
    let parent = folder.platform_handle();
    let file = CString::new(name).map_err(|_| invalid_name())?;
    let not_found = || ApiError::new(StatusCode::NOT_FOUND, "not_found", "no such uploaded file");
    let identity = metadata_at(parent, &file).map_err(|_| not_found())?;
    if !identity.regular {
        return Err(not_found());
    }
    remove_file_at(parent, &file).map_err(file_io)?;
    sync_directory(parent).map_err(file_io)
}

/// Deletes one uploaded file by the name the upload returned (`?name=`), or
/// cancels an upload by its id (`?id=`), deleting its file if it was already saved.
async fn remove(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<RemoveQuery>,
) -> Result<impl IntoResponse, ApiError> {
    authenticate(&headers, &state)?;
    let store = state.notebooks.clone();
    match (query.name, query.id) {
        (Some(name), None) => {
            if safe_name(&name)? != name {
                return Err(invalid_name());
            }
            let removed = name.clone();
            tokio::task::spawn_blocking(move || delete_saved(&store, &name))
                .await
                .map_err(|_| task_failed())??;
            Ok(Json(serde_json::json!({ "name": removed })))
        }
        (None, Some(id)) => {
            let key = (store.root.clone(), upload_id(&id)?);
            tokio::task::spawn_blocking(move || {
                let mut registry = uploads();
                if let Some(UploadState::Saved(name)) = registry.states.get(&key) {
                    match delete_saved(&store, &name.clone()) {
                        Ok(()) => {}
                        Err(error) if error.status == StatusCode::NOT_FOUND => {}
                        Err(error) => return Err(error),
                    }
                }
                registry.set(&key, UploadState::Cancelled);
                Ok(())
            })
            .await
            .map_err(|_| task_failed())??;
            Ok(Json(serde_json::json!({ "id": id, "cancelled": true })))
        }
        _ => Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "pass either name or id",
        )),
    }
}

#[cfg(test)]
mod tests;
