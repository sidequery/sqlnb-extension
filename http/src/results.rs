//! Result cache behind `sqlnb_result_tee` and its readers (docs/results.md).
//!
//! The registry is process-global. Results small enough stay in memory; the
//! others are Arrow IPC files in one session directory per process, which a
//! lock file claims for the life of the process.
use crate::platform::{
    disk_space, open_lease_file, open_lock_file, private_new_file, try_lock_file, DiskSpace, LOCK_EX,
};
use arrow_array::ffi::{from_ffi_and_data_type, FFI_ArrowArray, FFI_ArrowSchema};
use arrow_array::ffi_stream::FFI_ArrowArrayStream;
use arrow_array::{
    Array, ArrayRef, BooleanArray, Int64Array, RecordBatch, RecordBatchIterator,
    RecordBatchOptions, RecordBatchReader, StringArray, StructArray,
};
use arrow_data::ArrayData;
use arrow_ipc::convert::IpcSchemaEncoder;
use arrow_ipc::reader::{FileReader, FileReaderBuilder};
use arrow_ipc::writer::{write_message, DictionaryTracker, FileWriter, IpcDataGenerator, IpcWriteOptions};
use arrow_ipc::{Block, FooterBuilder, MetadataVersion};
use arrow_schema::{ArrowError, DataType, Field, Schema, SchemaRef};
use flatbuffers::FlatBufferBuilder;
use std::collections::HashMap;
use std::ffi::{c_char, CStr};
use std::fs::{self, File};
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// A result with at most `head_rows` rows and this many bytes of Arrow buffers
/// is kept in memory and never written to a file.
const MEMORY_RESULT_BYTES: u64 = 1024 * 1024;
const MAX_META_BYTES: usize = 64 * 1024;
const SESSION_LOCK: &str = "session.lock";
/// A live process locks its session directory moments after creating it, so an
/// unlocked directory older than this belongs to a process that is gone.
const SESSION_CLAIM_GRACE: Duration = Duration::from_secs(60);
/// Messages and their bodies start on this boundary in a result file.
const FILE_ALIGNMENT: u8 = 8;
/// What `sqlnb_result_tee` uses for a caller that names no limit.
const DEFAULT_MAX_RESULT_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const DEFAULT_MAX_CACHE_BYTES: u64 = 8 * 1024 * 1024 * 1024;
const DEFAULT_TTL_SECONDS: u64 = 24 * 60 * 60;
/// Nothing more is written to a result file while its volume has less free
/// space than this, or than a tenth of a volume too small for that.
const MIN_FREE_DISK_BYTES: u64 = 5 * 1024 * 1024 * 1024;
const MIN_FREE_DISK_DIVISOR: u64 = 10;
/// The free space is read again after this many bytes were written.
const DISK_PROBE_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LimitReason {
    ResultBytes,
    CacheBytes,
    DiskSpace,
    Io,
}

impl LimitReason {
    fn as_str(self) -> &'static str {
        match self {
            LimitReason::ResultBytes => "result-bytes",
            LimitReason::CacheBytes => "cache-bytes",
            LimitReason::DiskSpace => "disk-space",
            LimitReason::Io => "io",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ResultLimits {
    pub head_rows: u64,
    pub max_result_bytes: u64,
    pub max_cache_bytes: u64,
    /// How long the result is kept after its last use; `None` keeps it until
    /// the process ends.
    pub ttl_ms: Option<u64>,
}

impl ResultLimits {
    /// The limits of a caller that names none.
    pub(crate) fn defaults(head_rows: u64) -> Self {
        Self {
            head_rows,
            max_result_bytes: DEFAULT_MAX_RESULT_BYTES,
            max_cache_bytes: DEFAULT_MAX_CACHE_BYTES,
            ttl_ms: Some(DEFAULT_TTL_SECONDS * 1000),
        }
    }
}

pub(crate) struct ResultStore {
    /// Parent of the session directories; unavailable without a cache root.
    directory: Result<PathBuf, String>,
    state: Mutex<StoreState>,
    /// Milliseconds since the Unix epoch.
    clock: Box<dyn Fn() -> i64 + Send + Sync>,
    /// The space on the volume of a directory; `None` when it cannot be read.
    disk_space: Box<dyn Fn(&Path) -> Option<DiskSpace> + Send + Sync>,
    /// Bytes written to result files since the free space was last read.
    unprobed_bytes: AtomicU64,
    /// The last reading was below the floor: read again before every write.
    disk_low: AtomicBool,
}

#[derive(Default)]
struct StoreState {
    /// Databases that loaded the extension. The last one to close removes everything.
    retained: usize,
    session: Option<Session>,
    next_sequence: u64,
    entries: HashMap<String, Entry>,
}

struct Session {
    directory: PathBuf,
    /// Held open, and therefore locked, until the session ends.
    _lock: File,
}

struct Entry {
    sequence: u64,
    created_ms: i64,
    /// When the statement finished, or when the result was last read.
    last_used_ms: i64,
    ttl_ms: Option<u64>,
    meta: String,
    schema: SchemaRef,
    stored_bytes: u64,
    /// Set once the writer finished; an unfinished result is never readable.
    stored: Option<Arc<StoredResult>>,
}

struct StoredResult {
    rows: u64,
    /// False when the statement stopped reading at a limit: `rows` is then a lower bound.
    rows_exact: bool,
    stored_rows: u64,
    limit_reason: Option<LimitReason>,
    elapsed_ms: u64,
    /// The cumulative row count at the end of each stored batch.
    batch_ends: Vec<u64>,
    data: StoredData,
}

enum StoredData {
    Memory(Vec<RecordBatch>),
    File(PathBuf),
}

impl Entry {
    fn remove_file(&self) {
        if let Some(StoredData::File(path)) = self.stored.as_ref().map(|stored| &stored.data) {
            let _ = fs::remove_file(path);
        }
    }

    /// When a readable result stops being kept unless it is read before.
    fn expires_ms(&self) -> Option<i64> {
        let ttl_ms = i64::try_from(self.ttl_ms?).unwrap_or(i64::MAX);
        Some(self.last_used_ms.saturating_add(ttl_ms))
    }
}

impl StoreState {
    /// Takes out the readable results nobody used for their whole period. A
    /// result still being stored has not begun its period.
    fn take_expired(&mut self, now_ms: i64) -> Vec<Entry> {
        let expired: Vec<String> = self
            .entries
            .iter()
            .filter(|(_, entry)| {
                entry.stored.is_some() && entry.expires_ms().is_some_and(|expires| expires <= now_ms)
            })
            .map(|(id, _)| id.clone())
            .collect();
        expired.iter().filter_map(|id| self.entries.remove(id)).collect()
    }

    /// Takes out the oldest readable results other than `keep` until the cache
    /// holds at most `limit` bytes or has no other to give up. Returns the
    /// bytes it still holds with what it took out.
    fn evict_oldest(&mut self, keep: &str, limit: u64) -> (u64, Vec<Entry>) {
        let mut used: u64 = self.entries.values().map(|entry| entry.stored_bytes).sum();
        let mut evicted = Vec::new();
        while used > limit {
            let oldest = self
                .entries
                .iter()
                .filter(|(other, entry)| other.as_str() != keep && entry.stored.is_some())
                .min_by_key(|(_, entry)| entry.sequence)
                .map(|(other, _)| other.clone());
            let Some(oldest) = oldest else { break };
            let entry = self.entries.remove(&oldest).expect("oldest entry exists");
            used -= entry.stored_bytes;
            evicted.push(entry);
        }
        (used, evicted)
    }
}

impl ResultStore {
    pub(crate) fn new(directory: Result<PathBuf, String>) -> Self {
        Self::with_environment(directory, Box::new(unix_ms), Box::new(disk_space))
    }

    fn with_environment(
        directory: Result<PathBuf, String>,
        clock: Box<dyn Fn() -> i64 + Send + Sync>,
        disk_space: Box<dyn Fn(&Path) -> Option<DiskSpace> + Send + Sync>,
    ) -> Self {
        Self {
            directory,
            state: Mutex::new(StoreState::default()),
            clock,
            disk_space,
            unprobed_bytes: AtomicU64::new(0),
            disk_low: AtomicBool::new(false),
        }
    }

    fn lock(&self) -> MutexGuard<'_, StoreState> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Locks the registry once the results that expired are gone from it.
    /// Every call that lists, reads, drops or begins a result comes through
    /// here, which is what removes expired results: there is no timer.
    fn lock_swept(&self) -> MutexGuard<'_, StoreState> {
        let mut state = self.lock();
        let expired = state.take_expired((self.clock)());
        if expired.is_empty() {
            return state;
        }
        drop(state);
        for entry in &expired {
            entry.remove_file();
        }
        self.lock()
    }

    /// Whether `bytes` more may be written to a result file in `directory`.
    /// `first` is set for the write that creates the file.
    fn disk_room(&self, directory: &Path, bytes: u64, first: bool) -> Result<(), LimitReason> {
        let unprobed = self.unprobed_bytes.fetch_add(bytes, Ordering::Relaxed).saturating_add(bytes);
        if !first && unprobed < DISK_PROBE_BYTES && !self.disk_low.load(Ordering::Relaxed) {
            return Ok(());
        }
        self.unprobed_bytes.store(0, Ordering::Relaxed);
        // A volume that does not report its space is not held against the result.
        let low = (self.disk_space)(directory).is_some_and(|space| {
            let floor = MIN_FREE_DISK_BYTES.min(space.total / MIN_FREE_DISK_DIVISOR);
            space.available < floor.saturating_add(bytes)
        });
        self.disk_low.store(low, Ordering::Relaxed);
        if low {
            Err(LimitReason::DiskSpace)
        } else {
            Ok(())
        }
    }

    pub(crate) fn retain(&self) {
        self.lock().retained += 1;
    }

    pub(crate) fn release(&self) {
        let (entries, session) = {
            let mut state = self.lock();
            state.retained = state.retained.saturating_sub(1);
            if state.retained > 0 {
                return;
            }
            (std::mem::take(&mut state.entries), state.session.take())
        };
        for entry in entries.values() {
            entry.remove_file();
        }
        if let Some(session) = session {
            let directory = session.directory.clone();
            // Windows cannot remove a directory while the lock file is open.
            drop(session);
            let _ = fs::remove_dir_all(directory);
        }
    }

    /// Validates a result before its statement runs.
    pub(crate) fn check(&self, id: &str, meta: &str, schema: &Schema) -> Result<(), String> {
        validate_new_result(id, meta, schema)?;
        if self.lock_swept().entries.contains_key(id) {
            return Err(exists(id));
        }
        Ok(())
    }

    pub(crate) fn begin(
        self: &Arc<Self>,
        id: &str,
        meta: &str,
        schema: Schema,
        limits: ResultLimits,
    ) -> Result<ResultWriter, String> {
        validate_new_result(id, meta, &schema)?;
        let schema = Arc::new(schema);
        let evicted = {
            let mut state = self.lock_swept();
            if state.entries.contains_key(id) {
                return Err(exists(id));
            }
            state.next_sequence += 1;
            let now_ms = (self.clock)();
            let entry = Entry {
                sequence: state.next_sequence,
                created_ms: now_ms,
                last_used_ms: now_ms,
                ttl_ms: limits.ttl_ms,
                meta: meta.to_string(),
                schema: schema.clone(),
                stored_bytes: 0,
                stored: None,
            };
            state.entries.insert(id.to_string(), entry);
            // A cache limit lower than what is kept applies from here on.
            state.evict_oldest(id, limits.max_cache_bytes).1
        };
        for entry in evicted {
            entry.remove_file();
        }
        Ok(ResultWriter {
            store: self.clone(),
            id: id.to_string(),
            batch_type: DataType::Struct(schema.fields().clone()),
            schema,
            limits,
            started: Instant::now(),
            storing: AtomicBool::new(true),
            state: Mutex::new(WriterState::default()),
            finished: false,
        })
    }

    /// Counts `requested` more stored bytes for the result `id`, evicting the
    /// oldest other readable results when the whole cache would exceed its limit.
    fn reserve(&self, id: &str, requested: u64, limits: &ResultLimits) -> Result<(), LimitReason> {
        let (reserved, evicted) = {
            let mut state = self.lock();
            // Without its entry, the cache closed while this statement ran.
            let stored_bytes = state.entries.get(id).ok_or(LimitReason::Io)?.stored_bytes;
            if stored_bytes.saturating_add(requested) > limits.max_result_bytes {
                (Err(LimitReason::ResultBytes), Vec::new())
            } else {
                let (used, evicted) =
                    state.evict_oldest(id, limits.max_cache_bytes.saturating_sub(requested));
                if used.saturating_add(requested) > limits.max_cache_bytes {
                    (Err(LimitReason::CacheBytes), evicted)
                } else {
                    if let Some(entry) = state.entries.get_mut(id) {
                        entry.stored_bytes += requested;
                    }
                    (Ok(()), evicted)
                }
            }
        };
        for entry in evicted {
            entry.remove_file();
        }
        reserved
    }

    fn complete(&self, id: &str, stored: StoredResult, stored_bytes: u64) {
        let now_ms = (self.clock)();
        let mut state = self.lock();
        match state.entries.get_mut(id) {
            Some(entry) => {
                entry.stored_bytes = stored_bytes;
                entry.stored = Some(Arc::new(stored));
                // The period without use starts when the result can be read.
                entry.last_used_ms = now_ms;
            }
            None => {
                if let StoredData::File(path) = stored.data {
                    let _ = fs::remove_file(path);
                }
            }
        }
    }

    fn forget(&self, id: &str) {
        self.lock().entries.remove(id);
    }

    /// Creates this process's session directory on first use, after removing
    /// the directories of processes that are gone.
    fn session_directory(&self) -> Result<PathBuf, String> {
        let mut state = self.lock();
        if let Some(session) = &state.session {
            return Ok(session.directory.clone());
        }
        if state.retained == 0 {
            return Err("the result cache is closed".into());
        }
        let parent = self.directory.clone()?;
        fs::create_dir_all(&parent)
            .map_err(|error| format!("failed to create {}: {error}", parent.display()))?;
        remove_stale_sessions(&parent);
        let directory = parent.join(uuid::Uuid::new_v4().simple().to_string());
        create_private_directory(&directory)
            .map_err(|error| format!("failed to create {}: {error}", directory.display()))?;
        let lock = open_lock_file(&directory.join(SESSION_LOCK))?;
        if !try_lock_file(&lock, LOCK_EX, "result session")? {
            return Err("a new result session is locked by another process".into());
        }
        state.session = Some(Session {
            directory: directory.clone(),
            _lock: lock,
        });
        Ok(directory)
    }

    pub(crate) fn drop_result(&self, id: &str) -> bool {
        let removed = {
            let mut state = self.lock_swept();
            match state.entries.get(id) {
                Some(entry) if entry.stored.is_some() => state.entries.remove(id),
                _ => None,
            }
        };
        removed.map(|entry| entry.remove_file()).is_some()
    }

    /// A result about to be read, which counts as a use of it.
    fn readable(&self, id: &str) -> Result<(SchemaRef, Arc<StoredResult>), String> {
        let now_ms = (self.clock)();
        let mut state = self.lock_swept();
        let entry = state.entries.get_mut(id).ok_or_else(|| missing(id))?;
        let stored = entry.stored.clone().ok_or_else(|| missing(id))?;
        entry.last_used_ms = now_ms;
        Ok((entry.schema.clone(), stored))
    }

    /// The columns `scan` reads: the stored columns, then the optional row number.
    pub(crate) fn scan_schema(&self, id: &str, row_number: Option<&str>) -> Result<Schema, String> {
        let (schema, _) = self.readable(id)?;
        with_row_number(&schema, row_number)
    }

    /// Reads rows `[first_row, first_row + row_count)` of a stored result,
    /// projected to `columns` (indices into `scan_schema`).
    pub(crate) fn scan(
        &self,
        id: &str,
        first_row: u64,
        row_count: u64,
        row_number: Option<&str>,
        columns: &[usize],
    ) -> Result<StoredScan, String> {
        // Open the file before a concurrent drop can remove it; an open scan
        // keeps reading the file it opened.
        let (schema, stored, file) = {
            let now_ms = (self.clock)();
            let mut state = self.lock_swept();
            let entry = state.entries.get_mut(id).ok_or_else(|| missing(id))?;
            let stored = entry.stored.clone().ok_or_else(|| missing(id))?;
            let file = match &stored.data {
                StoredData::File(path) => Some(File::open(path).map_err(|_| missing(id))?),
                StoredData::Memory(_) => None,
            };
            entry.last_used_ms = now_ms;
            (entry.schema.clone(), stored, file)
        };
        let full_schema = with_row_number(&schema, row_number)?;
        let stored_count = schema.fields().len();
        let mut stored_columns: Vec<usize> = columns
            .iter()
            .copied()
            .filter(|&column| column < stored_count)
            .collect();
        stored_columns.sort_unstable();
        stored_columns.dedup();
        let mut fields = Vec::with_capacity(columns.len());
        let mut sources = Vec::with_capacity(columns.len());
        for &column in columns {
            let field = full_schema
                .fields()
                .get(column)
                .ok_or_else(|| format!("result column {column} does not exist"))?;
            fields.push(field.clone());
            sources.push(match stored_columns.binary_search(&column) {
                Ok(position) => ColumnSource::Stored(position),
                Err(_) => ColumnSource::RowNumber,
            });
        }
        let source = match file {
            Some(file) => BatchSource::File(Box::new(
                FileReaderBuilder::new()
                    .with_projection(stored_columns)
                    .build(BufReader::new(file))
                    .map_err(|error| format!("failed to read stored result {id}: {error}"))?,
            )),
            None => BatchSource::Memory(stored_columns),
        };
        let first_row = first_row.min(stored.stored_rows);
        let end_row = first_row.saturating_add(row_count).min(stored.stored_rows);
        let mut scan = StoredScan {
            schema: Arc::new(Schema::new(fields)),
            sources,
            next_batch: stored.batch_ends.partition_point(|&end| end <= first_row),
            stored,
            source,
            position: first_row,
            end_row,
        };
        if let BatchSource::File(reader) = &mut scan.source {
            if scan.position < scan.end_row {
                reader
                    .set_index(scan.next_batch)
                    .map_err(|error| format!("failed to read stored result {id}: {error}"))?;
            }
        }
        Ok(scan)
    }

    /// One row per readable result, newest first. Listing is not a use.
    pub(crate) fn list(&self) -> RecordBatch {
        let state = self.lock_swept();
        let mut results: Vec<_> = state
            .entries
            .iter()
            .filter_map(|(id, entry)| Some((id, entry, entry.stored.as_ref()?)))
            .collect();
        results.sort_by_key(|(_, entry, _)| std::cmp::Reverse(entry.sequence));
        let int64 = |value: fn(&Entry, &StoredResult) -> u64| -> ArrayRef {
            Arc::new(Int64Array::from_iter_values(results.iter().map(|(_, entry, stored)| {
                i64::try_from(value(entry, stored)).unwrap_or(i64::MAX)
            })))
        };
        let columns: Vec<ArrayRef> = vec![
            Arc::new(StringArray::from_iter_values(results.iter().map(|(id, _, _)| id.as_str()))),
            int64(|_, stored| stored.rows),
            int64(|_, stored| stored.stored_rows),
            int64(|entry, _| entry.stored_bytes),
            Arc::new(BooleanArray::from_iter(results.iter().map(|(_, _, stored)| {
                Some(matches!(stored.data, StoredData::Memory(_)))
            }))),
            Arc::new(StringArray::from_iter(results.iter().map(|(_, _, stored)| {
                stored.limit_reason.map(LimitReason::as_str)
            }))),
            Arc::new(StringArray::from_iter_values(results.iter().map(|(_, entry, _)| entry.meta.as_str()))),
            Arc::new(Int64Array::from_iter_values(results.iter().map(|(_, entry, _)| entry.created_ms))),
            int64(|_, stored| stored.elapsed_ms),
            Arc::new(BooleanArray::from_iter(
                results.iter().map(|(_, _, stored)| Some(stored.rows_exact)),
            )),
            Arc::new(Int64Array::from_iter_values(results.iter().map(|(_, entry, _)| entry.last_used_ms))),
            Arc::new(Int64Array::from_iter(results.iter().map(|(_, entry, _)| entry.expires_ms()))),
        ];
        RecordBatch::try_new(list_schema(), columns).expect("result list matches its schema")
    }
}

fn list_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("id", DataType::Utf8, false),
        Field::new("rows", DataType::Int64, false),
        Field::new("stored_rows", DataType::Int64, false),
        Field::new("stored_bytes", DataType::Int64, false),
        Field::new("in_memory", DataType::Boolean, false),
        Field::new("limit_reason", DataType::Utf8, true),
        Field::new("meta", DataType::Utf8, false),
        Field::new("created_ms", DataType::Int64, false),
        Field::new("elapsed_ms", DataType::Int64, false),
        Field::new("rows_exact", DataType::Boolean, false),
        Field::new("last_used_ms", DataType::Int64, false),
        Field::new("expires_ms", DataType::Int64, true),
    ]))
}

pub(crate) fn valid_id(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

fn validate_new_result(id: &str, meta: &str, schema: &Schema) -> Result<(), String> {
    if !valid_id(id) {
        return Err(format!("result id '{id}' must be 32 lowercase hexadecimal characters"));
    }
    if meta.len() > MAX_META_BYTES {
        return Err(format!("result meta exceeds {MAX_META_BYTES} bytes"));
    }
    // Encoding the schema rejects every type the IPC file format cannot hold.
    FileWriter::try_new_with_options(std::io::sink(), schema, write_options())
        .map(|_| ())
        .map_err(|error| format!("SQLNB_RESULT_UNSUPPORTED: {error}"))
}

fn exists(id: &str) -> String {
    format!("SQLNB_RESULT_EXISTS: result '{id}' already exists")
}

fn missing(id: &str) -> String {
    format!("SQLNB_RESULT_MISSING: result '{id}' does not exist, expired or was evicted")
}

fn with_row_number(schema: &Schema, row_number: Option<&str>) -> Result<Schema, String> {
    let mut fields = schema.fields().to_vec();
    if let Some(name) = row_number {
        let lowered = name.to_lowercase();
        if name.is_empty() || fields.iter().any(|field| field.name().to_lowercase() == lowered) {
            return Err(format!("row_number column '{name}' must be a new, non-empty column name"));
        }
        fields.push(Arc::new(Field::new(name, DataType::Int64, false)));
    }
    Ok(Schema::new_with_metadata(fields, schema.metadata().clone()))
}

/// Uncompressed: LZ4 frames made files 2.6 times smaller on measured data but
/// writing 35% and full scans 40% slower (docs/results.md).
fn write_options() -> IpcWriteOptions {
    IpcWriteOptions::try_new(FILE_ALIGNMENT as usize, false, MetadataVersion::V5)
        .expect("the result file options are valid")
}

fn unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as i64)
}

fn create_private_directory(path: &Path) -> std::io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(path)
}

fn remove_stale_sessions(parent: &Path) {
    let Ok(sessions) = fs::read_dir(parent) else {
        return;
    };
    for session in sessions.flatten() {
        if session.file_type().is_ok_and(|kind| kind.is_dir()) && session_is_stale(&session.path()) {
            let _ = fs::remove_dir_all(session.path());
        }
    }
}

fn session_is_stale(directory: &Path) -> bool {
    let claimed_before_grace = |path: &Path| {
        fs::metadata(path)
            .and_then(|metadata| metadata.modified())
            .is_ok_and(|modified| modified.elapsed().is_ok_and(|age| age >= SESSION_CLAIM_GRACE))
    };
    let lock_path = directory.join(SESSION_LOCK);
    match open_lease_file(&lock_path) {
        // The process stopped between creating the directory and its lock.
        Err(_) => claimed_before_grace(directory),
        // The lock is released, and the file closed, when this function returns.
        Ok(lock) => {
            claimed_before_grace(&lock_path)
                && try_lock_file(&lock, LOCK_EX, "result session").unwrap_or(false)
        }
    }
}

/// Sum of the Arrow buffer bytes of a batch.
fn batch_bytes(batch: &RecordBatch) -> u64 {
    fn data_bytes(data: &ArrayData) -> u64 {
        data.buffers().iter().map(|buffer| buffer.len() as u64).sum::<u64>()
            + data.nulls().map_or(0, |nulls| nulls.buffer().len() as u64)
            + data.child_data().iter().map(data_bytes).sum::<u64>()
    }
    batch.columns().iter().map(|column| data_bytes(&column.to_data())).sum()
}

/// Where a batch belongs in its result. Batches are stored as they arrive,
/// from any thread, and read back in the order of their keys.
pub(crate) type BatchKey = (u64, u64);

const FILE_MAGIC: &[u8; 6] = b"ARROW1";

/// The start of an Arrow IPC file: the magic, padded, then the schema message.
fn file_header(schema: &Schema) -> Result<Vec<u8>, ArrowError> {
    let options = write_options();
    let mut bytes = FILE_MAGIC.to_vec();
    bytes.resize(FILE_ALIGNMENT as usize, 0);
    let schema = IpcDataGenerator::default().schema_to_bytes_with_dictionary_tracker(
        schema,
        &mut DictionaryTracker::new(true),
        &options,
    );
    write_message(&mut bytes, schema, &options)?;
    Ok(bytes)
}

/// One record batch message, with the lengths its footer block records.
fn encode_batch(batch: &RecordBatch) -> Result<(Vec<u8>, usize, usize), ArrowError> {
    let options = write_options();
    let (dictionaries, encoded) = IpcDataGenerator::default().encode(
        batch,
        &mut DictionaryTracker::new(true),
        &options,
        &mut Default::default(),
    )?;
    if !dictionaries.is_empty() {
        // A dictionary would have to precede every batch that uses it.
        return Err(ArrowError::IpcError("dictionary-encoded columns are not stored".into()));
    }
    let mut bytes = Vec::with_capacity(encoded.ipc_message.len() + encoded.arrow_data.len() + 64);
    let (metadata, body) = write_message(&mut bytes, encoded, &options)?;
    Ok((bytes, metadata, body))
}

/// The end of an Arrow IPC file. The footer lists the batches in the order
/// they are read, which is how batches written out of order become one result.
fn file_footer(schema: &Schema, blocks: &[Block]) -> Vec<u8> {
    // The end-of-stream marker Arrow's own writer puts before the footer.
    let mut bytes = vec![0xFF, 0xFF, 0xFF, 0xFF, 0, 0, 0, 0];
    let mut builder = FlatBufferBuilder::new();
    let dictionaries = builder.create_vector::<Block>(&[]);
    let record_batches = builder.create_vector(blocks);
    let mut tracker = DictionaryTracker::new(true);
    let schema = IpcSchemaEncoder::new()
        .with_dictionary_tracker(&mut tracker)
        .schema_to_fb_offset(&mut builder, schema);
    let root = {
        let mut footer = FooterBuilder::new(&mut builder);
        footer.add_version(MetadataVersion::V5);
        footer.add_schema(schema);
        footer.add_dictionaries(dictionaries);
        footer.add_recordBatches(record_batches);
        footer.finish()
    };
    builder.finish(root, None);
    let footer = builder.finished_data();
    bytes.extend_from_slice(footer);
    bytes.extend_from_slice(&(footer.len() as i32).to_le_bytes());
    bytes.extend_from_slice(FILE_MAGIC);
    bytes
}

#[cfg(unix)]
fn write_all_at(file: &File, bytes: &[u8], offset: u64) -> std::io::Result<()> {
    std::os::unix::fs::FileExt::write_all_at(file, bytes, offset)
}

#[cfg(windows)]
fn write_all_at(file: &File, mut bytes: &[u8], mut offset: u64) -> std::io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !bytes.is_empty() {
        let written = file.seek_write(bytes, offset)?;
        if written == 0 {
            return Err(std::io::ErrorKind::WriteZero.into());
        }
        bytes = &bytes[written..];
        offset += written as u64;
    }
    Ok(())
}

struct SpillFile {
    file: File,
    directory: PathBuf,
    temporary: PathBuf,
    path: PathBuf,
    /// The first free byte. A batch claims its place here and is written
    /// there without a lock, so threads write at the same time.
    end: AtomicU64,
}

impl SpillFile {
    fn create(directory: &Path, id: &str, schema: &Schema) -> Result<Self, String> {
        let header = file_header(schema).map_err(|error| error.to_string())?;
        let temporary = directory.join(format!("{id}.arrow.tmp"));
        let file = private_new_file(&temporary)
            .map_err(|error| format!("failed to create {}: {error}", temporary.display()))?;
        if let Err(error) = write_all_at(&file, &header, 0) {
            drop(file);
            let _ = fs::remove_file(&temporary);
            return Err(error.to_string());
        }
        Ok(Self {
            file,
            directory: directory.to_path_buf(),
            temporary,
            path: directory.join(format!("{id}.arrow")),
            end: AtomicU64::new(header.len() as u64),
        })
    }

    fn write(&self, batch: &RecordBatch) -> Result<Block, LimitReason> {
        let (bytes, metadata, body) = encode_batch(batch).map_err(|_| LimitReason::Io)?;
        let offset = self.end.fetch_add(bytes.len() as u64, Ordering::Relaxed);
        write_all_at(&self.file, &bytes, offset).map_err(|_| LimitReason::Io)?;
        Ok(Block::new(offset as i64, metadata as i32, body as i64))
    }

    /// Writes the footer and publishes the file under its final name.
    fn finish(self, schema: &Schema, blocks: &[Block]) -> Result<(PathBuf, u64), String> {
        let temporary = self.temporary.clone();
        let result = (|| {
            let footer = file_footer(schema, blocks);
            let end = self.end.load(Ordering::Relaxed);
            write_all_at(&self.file, &footer, end).map_err(|error| error.to_string())?;
            // Windows cannot rename a file that is still open.
            drop(self.file);
            fs::rename(&self.temporary, &self.path).map_err(|error| error.to_string())?;
            Ok((self.path, end + footer.len() as u64))
        })();
        if result.is_err() {
            let _ = fs::remove_file(temporary);
        }
        result
    }

    fn discard(self) {
        drop(self.file);
        let _ = fs::remove_file(self.temporary);
    }
}

struct StoredBatch {
    key: BatchKey,
    rows: u64,
    bytes: u64,
    data: BatchData,
}

enum BatchData {
    Memory(RecordBatch),
    File(Block),
}

#[derive(Default)]
struct WriterState {
    /// In the order they arrived.
    batches: Vec<StoredBatch>,
    file: Option<Arc<SpillFile>>,
    stored_rows: u64,
    stored_bytes: u64,
    limit_reason: Option<LimitReason>,
    /// The first batch of the result that was not stored. The result keeps the
    /// batches before it.
    first_missing: Option<BatchKey>,
}

/// Stores one statement's rows; any thread may append. Dropping an unfinished
/// writer discards the result.
pub(crate) struct ResultWriter {
    store: Arc<ResultStore>,
    id: String,
    schema: SchemaRef,
    batch_type: DataType,
    limits: ResultLimits,
    started: Instant,
    /// Cleared once a limit or a failure ends storing.
    storing: AtomicBool,
    state: Mutex<WriterState>,
    finished: bool,
}

impl ResultWriter {
    fn lock(&self) -> MutexGuard<'_, WriterState> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Imports and stores a batch exported through the C Data Interface.
    fn append_ffi(&self, array: FFI_ArrowArray, key: BatchKey) -> bool {
        if !self.storing.load(Ordering::Relaxed) {
            self.missing(key, LimitReason::Io);
            return false;
        }
        let batch = unsafe { from_ffi_and_data_type(array, self.batch_type.clone()) }.and_then(|data| {
            RecordBatch::try_new(self.schema.clone(), StructArray::from(data).into_parts().1)
        });
        match batch {
            Ok(batch) => self.append(batch, key),
            Err(_) => {
                self.missing(key, LimitReason::Io);
                false
            }
        }
    }

    /// Stores a batch whole, or not at all once a limit is reached. Returns
    /// whether later batches can still be stored.
    pub(crate) fn append(&self, batch: RecordBatch, key: BatchKey) -> bool {
        let rows = batch.num_rows() as u64;
        if rows == 0 {
            return self.storing.load(Ordering::Relaxed);
        }
        let stored = if self.storing.load(Ordering::Relaxed) {
            let bytes = batch_bytes(&batch);
            self.store
                .reserve(&self.id, bytes, &self.limits)
                .and_then(|()| self.store_batch(batch, key, rows, bytes))
        } else {
            Err(LimitReason::Io)
        };
        match stored {
            Ok(()) => true,
            Err(reason) => {
                self.missing(key, reason);
                false
            }
        }
    }

    fn store_batch(&self, batch: RecordBatch, key: BatchKey, rows: u64, bytes: u64) -> Result<(), LimitReason> {
        let file = {
            let mut state = self.lock();
            if state.file.is_none() {
                if state.stored_rows + rows <= self.limits.head_rows
                    && state.stored_bytes + bytes <= MEMORY_RESULT_BYTES
                {
                    state.stored_rows += rows;
                    state.stored_bytes += bytes;
                    state.batches.push(StoredBatch { key, rows, bytes, data: BatchData::Memory(batch) });
                    return Ok(());
                }
                self.spill(&mut state, bytes)?;
            } else {
                let directory = &state.file.as_ref().expect("checked above").directory;
                self.store.disk_room(directory, bytes, false)?;
            }
            state.file.clone().expect("a spilled result has a file")
        };
        // Encoded and written outside the lock: other threads do the same.
        let block = file.write(&batch)?;
        let mut state = self.lock();
        state.stored_rows += rows;
        state.stored_bytes += bytes;
        state.batches.push(StoredBatch { key, rows, bytes, data: BatchData::File(block) });
        Ok(())
    }

    /// Moves the batches kept in memory into a new file that `incoming` more
    /// bytes are about to follow into. On failure they stay in memory as what
    /// the result keeps.
    fn spill(&self, state: &mut WriterState, incoming: u64) -> Result<(), LimitReason> {
        let directory = self.store.session_directory().map_err(|_| LimitReason::Io)?;
        self.store.disk_room(&directory, state.stored_bytes.saturating_add(incoming), true)?;
        let file = SpillFile::create(&directory, &self.id, &self.schema).map_err(|_| LimitReason::Io)?;
        let mut blocks = Vec::with_capacity(state.batches.len());
        for stored in &state.batches {
            let BatchData::Memory(batch) = &stored.data else {
                unreachable!("a result without a file keeps its batches in memory")
            };
            match file.write(batch) {
                Ok(block) => blocks.push(block),
                Err(reason) => {
                    file.discard();
                    return Err(reason);
                }
            }
        }
        for (stored, block) in state.batches.iter_mut().zip(blocks) {
            stored.data = BatchData::File(block);
        }
        state.file = Some(Arc::new(file));
        Ok(())
    }

    /// Records that the batch `key` is not stored, which ends storing: the
    /// result keeps the batches before the first missing one.
    pub(crate) fn missing(&self, key: BatchKey, reason: LimitReason) {
        let mut state = self.lock();
        state.limit_reason.get_or_insert(reason);
        state.first_missing = Some(state.first_missing.map_or(key, |first| first.min(key)));
        self.storing.store(false, Ordering::Relaxed);
    }

    /// Makes the result readable. `rows` is the number of rows the statement
    /// produced, or only those it read when it stopped early (`rows_exact` false).
    pub(crate) fn finish(mut self, rows: u64, rows_exact: bool) {
        self.finished = true;
        let elapsed_ms = self.started.elapsed().as_millis() as u64;
        let mut state = std::mem::take(&mut *self.lock());
        state.batches.sort_by_key(|stored| stored.key);
        if let Some(first_missing) = state.first_missing {
            let kept = state.batches.partition_point(|stored| stored.key < first_missing);
            state.batches.truncate(kept);
        }
        let mut stored_rows = 0;
        let mut stored_bytes = 0;
        let mut batch_ends = Vec::with_capacity(state.batches.len());
        for stored in &state.batches {
            stored_rows += stored.rows;
            stored_bytes += stored.bytes;
            batch_ends.push(stored_rows);
        }
        let mut limit_reason = state.limit_reason;
        let data = match state.file.take() {
            None => StoredData::Memory(
                state
                    .batches
                    .into_iter()
                    .filter_map(|stored| match stored.data {
                        BatchData::Memory(batch) => Some(batch),
                        BatchData::File(_) => None,
                    })
                    .collect(),
            ),
            Some(file) => {
                let blocks: Vec<Block> = state
                    .batches
                    .iter()
                    .filter_map(|stored| match &stored.data {
                        BatchData::File(block) => Some(*block),
                        BatchData::Memory(_) => None,
                    })
                    .collect();
                // Every append has returned, so this is the only reference left.
                let published = Arc::try_unwrap(file)
                    .map_err(|_| "the result file is still being written".to_string())
                    .and_then(|file| file.finish(&self.schema, &blocks));
                match published {
                    Ok((path, bytes)) => {
                        stored_bytes = bytes;
                        StoredData::File(path)
                    }
                    Err(_) => {
                        stored_rows = 0;
                        stored_bytes = 0;
                        batch_ends.clear();
                        limit_reason = Some(LimitReason::Io);
                        StoredData::Memory(Vec::new())
                    }
                }
            }
        };
        let stored = StoredResult {
            rows,
            rows_exact,
            stored_rows,
            limit_reason,
            elapsed_ms,
            batch_ends,
            data,
        };
        self.store.complete(&self.id, stored, stored_bytes);
    }
}

impl Drop for ResultWriter {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        let file = self.lock().file.take();
        if let Some(file) = file.and_then(Arc::into_inner) {
            file.discard();
        }
        self.store.forget(&self.id);
    }
}

enum ColumnSource {
    /// Position among the stored columns the scan reads.
    Stored(usize),
    RowNumber,
}

enum BatchSource {
    /// The stored columns to project from each batch.
    Memory(Vec<usize>),
    File(Box<FileReader<BufReader<File>>>),
}

/// A window of a stored result, read batch by batch.
pub(crate) struct StoredScan {
    schema: SchemaRef,
    sources: Vec<ColumnSource>,
    stored: Arc<StoredResult>,
    source: BatchSource,
    next_batch: usize,
    position: u64,
    end_row: u64,
}

impl StoredScan {
    fn read_batch(&mut self) -> Result<RecordBatch, ArrowError> {
        let index = self.next_batch;
        self.next_batch += 1;
        match &mut self.source {
            BatchSource::Memory(columns) => match &self.stored.data {
                StoredData::Memory(batches) => batches[index].project(columns),
                StoredData::File(_) => unreachable!("memory scans read memory results"),
            },
            BatchSource::File(reader) => reader.next().unwrap_or_else(|| {
                Err(ArrowError::IpcError("stored result has fewer batches than recorded".into()))
            }),
        }
    }
}

impl Iterator for StoredScan {
    type Item = Result<RecordBatch, ArrowError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.position >= self.end_row {
            return None;
        }
        let index = self.next_batch;
        let batch_start = index.checked_sub(1).map_or(0, |previous| self.stored.batch_ends[previous]);
        let batch_end = self.stored.batch_ends[index];
        let batch = match self.read_batch() {
            Ok(batch) => batch,
            Err(error) => return Some(Err(error)),
        };
        let from = self.position - batch_start;
        let length = batch_end.min(self.end_row) - self.position;
        let batch = batch.slice(from as usize, length as usize);
        let columns = self
            .sources
            .iter()
            .map(|source| match source {
                ColumnSource::Stored(position) => batch.column(*position).clone(),
                ColumnSource::RowNumber => Arc::new(Int64Array::from_iter_values(
                    (self.position..self.position + length).map(|row| row as i64),
                )) as ArrayRef,
            })
            .collect();
        self.position += length;
        Some(RecordBatch::try_new_with_options(
            self.schema.clone(),
            columns,
            &RecordBatchOptions::new().with_row_count(Some(length as usize)),
        ))
    }
}

impl RecordBatchReader for StoredScan {
    fn schema(&self) -> SchemaRef {
        self.schema.clone()
    }
}

static STORE: LazyLock<Arc<ResultStore>> =
    LazyLock::new(|| Arc::new(ResultStore::new(crate::cache_location::results_directory())));

/// Runs an exported function, turning errors and panics into `false` and `*error_out`.
fn exported(error_out: *mut *mut c_char, body: impl FnOnce() -> Result<(), String>) -> bool {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)) {
        Ok(Ok(())) => true,
        Ok(Err(error)) => {
            crate::set_error(error_out, error);
            false
        }
        Err(_) => {
            crate::set_error(error_out, "unexpected panic in the result cache".into());
            false
        }
    }
}

/// Reads a required UTF-8 argument; null pointers are rejected as absent.
unsafe fn text<'a>(value: *const c_char, name: &str) -> Result<&'a str, String> {
    if value.is_null() {
        return Err(format!("{name} must not be null"));
    }
    unsafe { CStr::from_ptr(value) }
        .to_str()
        .map_err(|_| format!("{name} must be UTF-8"))
}

unsafe fn optional_text<'a>(value: *const c_char, name: &str) -> Result<Option<&'a str>, String> {
    if value.is_null() {
        Ok(None)
    } else {
        unsafe { text(value, name) }.map(Some)
    }
}

unsafe fn import_schema(schema: *mut FFI_ArrowSchema) -> Result<Schema, String> {
    if schema.is_null() {
        return Err("result schema must not be null".into());
    }
    let schema = unsafe { FFI_ArrowSchema::from_raw(schema) };
    Schema::try_from(&schema).map_err(|error| format!("SQLNB_RESULT_UNSUPPORTED: {error}"))
}

#[no_mangle]
pub extern "C" fn sidequery_results_retain() {
    let _ = std::panic::catch_unwind(|| STORE.retain());
}

#[no_mangle]
pub extern "C" fn sidequery_results_release() {
    let _ = std::panic::catch_unwind(|| STORE.release());
}

#[no_mangle]
pub unsafe extern "C" fn sidequery_result_defaults(
    max_result_bytes: *mut u64,
    max_cache_bytes: *mut u64,
    ttl_seconds: *mut u64,
) {
    let defaults = ResultLimits::defaults(0);
    unsafe {
        max_result_bytes.write(defaults.max_result_bytes);
        max_cache_bytes.write(defaults.max_cache_bytes);
        ttl_seconds.write(defaults.ttl_ms.map_or(0, |ttl_ms| ttl_ms / 1000));
    }
}

#[no_mangle]
pub unsafe extern "C" fn sidequery_result_check(
    id: *const c_char,
    meta: *const c_char,
    schema: *mut FFI_ArrowSchema,
    error_out: *mut *mut c_char,
) -> bool {
    exported(error_out, || {
        let schema = unsafe { import_schema(schema) }?;
        STORE.check(unsafe { text(id, "id") }?, unsafe { text(meta, "meta") }?, &schema)
    })
}

#[no_mangle]
pub unsafe extern "C" fn sidequery_result_begin(
    id: *const c_char,
    meta: *const c_char,
    schema: *mut FFI_ArrowSchema,
    head_rows: u64,
    max_result_bytes: u64,
    max_cache_bytes: u64,
    ttl_seconds: u64,
    error_out: *mut *mut c_char,
) -> *mut ResultWriter {
    let mut writer = None;
    exported(error_out, || {
        let schema = unsafe { import_schema(schema) }?;
        let limits = ResultLimits {
            head_rows,
            max_result_bytes,
            max_cache_bytes,
            ttl_ms: (ttl_seconds > 0).then(|| ttl_seconds.saturating_mul(1000)),
        };
        writer = Some(STORE.begin(unsafe { text(id, "id") }?, unsafe { text(meta, "meta") }?, schema, limits)?);
        Ok(())
    });
    writer.map_or(std::ptr::null_mut(), |writer| Box::into_raw(Box::new(writer)))
}

#[no_mangle]
pub unsafe extern "C" fn sidequery_result_append(
    writer: *const ResultWriter,
    batch: *mut FFI_ArrowArray,
    major: u64,
    minor: u64,
) -> bool {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let writer = unsafe { &*writer };
        writer.append_ffi(unsafe { FFI_ArrowArray::from_raw(batch) }, (major, minor))
    }))
    .unwrap_or(false)
}

#[no_mangle]
pub unsafe extern "C" fn sidequery_result_missing(writer: *const ResultWriter, major: u64, minor: u64) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        unsafe { &*writer }.missing((major, minor), LimitReason::Io)
    }));
}

#[no_mangle]
pub unsafe extern "C" fn sidequery_result_finish(
    writer: *mut ResultWriter,
    rows: u64,
    rows_exact: bool,
) {
    let writer = unsafe { Box::from_raw(writer) };
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| writer.finish(rows, rows_exact)));
}

#[no_mangle]
pub unsafe extern "C" fn sidequery_result_abort(writer: *mut ResultWriter) {
    let writer = unsafe { Box::from_raw(writer) };
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(writer)));
}

#[no_mangle]
pub unsafe extern "C" fn sidequery_results_list(
    out: *mut FFI_ArrowArrayStream,
    error_out: *mut *mut c_char,
) -> bool {
    exported(error_out, || {
        let reader = RecordBatchIterator::new([Ok(STORE.list())], list_schema());
        unsafe { out.write(FFI_ArrowArrayStream::new(Box::new(reader))) };
        Ok(())
    })
}

#[no_mangle]
pub unsafe extern "C" fn sidequery_result_schema(
    id: *const c_char,
    row_number: *const c_char,
    out: *mut FFI_ArrowSchema,
    error_out: *mut *mut c_char,
) -> bool {
    exported(error_out, || {
        let schema = STORE.scan_schema(
            unsafe { text(id, "id") }?,
            unsafe { optional_text(row_number, "row_number") }?,
        )?;
        let schema = FFI_ArrowSchema::try_from(&schema).map_err(|error| error.to_string())?;
        unsafe { out.write(schema) };
        Ok(())
    })
}

#[no_mangle]
pub unsafe extern "C" fn sidequery_result_scan(
    id: *const c_char,
    first_row: u64,
    row_count: u64,
    row_number: *const c_char,
    columns: *const u64,
    column_count: u64,
    out: *mut FFI_ArrowArrayStream,
    error_out: *mut *mut c_char,
) -> bool {
    exported(error_out, || {
        let columns: Vec<usize> = if column_count == 0 {
            Vec::new()
        } else {
            unsafe { std::slice::from_raw_parts(columns, column_count as usize) }
                .iter()
                .map(|&column| column as usize)
                .collect()
        };
        let scan = STORE.scan(
            unsafe { text(id, "id") }?,
            first_row,
            row_count,
            unsafe { optional_text(row_number, "row_number") }?,
            &columns,
        )?;
        unsafe { out.write(FFI_ArrowArrayStream::new(Box::new(scan))) };
        Ok(())
    })
}

#[no_mangle]
pub unsafe extern "C" fn sidequery_result_drop(id: *const c_char) -> bool {
    std::panic::catch_unwind(|| unsafe { text(id, "id") }.is_ok_and(|id| STORE.drop_result(id)))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests;
