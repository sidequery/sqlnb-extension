//! Result cache behind `sqlnb_result_tee` and its readers (docs/results.md).
//!
//! The registry is process-global. Results small enough stay in memory; the
//! others are Arrow IPC files in one session directory per process, which a
//! lock file claims for the life of the process.
use crate::platform::{open_lease_file, open_lock_file, private_new_file, try_lock_file, LOCK_EX};
use arrow_array::ffi::{from_ffi_and_data_type, FFI_ArrowArray, FFI_ArrowSchema};
use arrow_array::ffi_stream::FFI_ArrowArrayStream;
use arrow_array::{
    Array, ArrayRef, BooleanArray, Int64Array, RecordBatch, RecordBatchIterator,
    RecordBatchOptions, RecordBatchReader, StringArray, StructArray,
};
use arrow_data::ArrayData;
use arrow_ipc::reader::{FileReader, FileReaderBuilder};
use arrow_ipc::writer::{FileWriter, IpcWriteOptions};
use arrow_schema::{ArrowError, DataType, Field, Schema, SchemaRef};
use std::collections::HashMap;
use std::ffi::{c_char, CStr};
use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
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
const FILE_BUFFER_BYTES: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LimitReason {
    ResultBytes,
    CacheBytes,
    Io,
}

impl LimitReason {
    fn as_str(self) -> &'static str {
        match self {
            LimitReason::ResultBytes => "result-bytes",
            LimitReason::CacheBytes => "cache-bytes",
            LimitReason::Io => "io",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ResultLimits {
    pub head_rows: u64,
    pub max_result_bytes: u64,
    pub max_cache_bytes: u64,
}

pub(crate) struct ResultStore {
    /// Parent of the session directories; unavailable without a cache root.
    directory: Result<PathBuf, String>,
    state: Mutex<StoreState>,
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
}

impl ResultStore {
    pub(crate) fn new(directory: Result<PathBuf, String>) -> Self {
        Self {
            directory,
            state: Mutex::new(StoreState::default()),
        }
    }

    fn lock(&self) -> MutexGuard<'_, StoreState> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
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
        if self.lock().entries.contains_key(id) {
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
        {
            let mut state = self.lock();
            if state.entries.contains_key(id) {
                return Err(exists(id));
            }
            state.next_sequence += 1;
            let entry = Entry {
                sequence: state.next_sequence,
                created_ms: unix_ms(),
                meta: meta.to_string(),
                schema: schema.clone(),
                stored_bytes: 0,
                stored: None,
            };
            state.entries.insert(id.to_string(), entry);
        }
        Ok(ResultWriter {
            store: self.clone(),
            id: id.to_string(),
            batch_type: DataType::Struct(schema.fields().clone()),
            schema,
            limits,
            started: Instant::now(),
            stored_rows: 0,
            stored_bytes: 0,
            batch_ends: Vec::new(),
            memory: Vec::new(),
            file: None,
            limit_reason: None,
            finished: false,
        })
    }

    /// Grants up to `requested` more stored bytes to the result `id`, evicting the
    /// oldest other readable results when the whole cache would exceed its limit.
    fn reserve(
        &self,
        id: &str,
        stored_bytes: u64,
        requested: u64,
        limits: &ResultLimits,
    ) -> (u64, Option<LimitReason>) {
        let result_room = limits.max_result_bytes.saturating_sub(stored_bytes);
        let needed = requested.min(result_room);
        let mut evicted = Vec::new();
        let granted = {
            let mut state = self.lock();
            if !state.entries.contains_key(id) {
                // The cache closed while this statement ran.
                return (0, Some(LimitReason::Io));
            }
            let mut used: u64 = state.entries.values().map(|entry| entry.stored_bytes).sum();
            while used.saturating_add(needed) > limits.max_cache_bytes {
                let oldest = state
                    .entries
                    .iter()
                    .filter(|(other, entry)| other.as_str() != id && entry.stored.is_some())
                    .min_by_key(|(_, entry)| entry.sequence)
                    .map(|(other, _)| other.clone());
                let Some(oldest) = oldest else { break };
                let entry = state.entries.remove(&oldest).expect("oldest entry exists");
                used -= entry.stored_bytes;
                evicted.push(entry);
            }
            needed.min(limits.max_cache_bytes.saturating_sub(used))
        };
        for entry in evicted {
            entry.remove_file();
        }
        let reason = if granted == requested {
            None
        } else if granted == result_room {
            Some(LimitReason::ResultBytes)
        } else {
            Some(LimitReason::CacheBytes)
        };
        (granted, reason)
    }

    fn record_stored_bytes(&self, id: &str, stored_bytes: u64) {
        if let Some(entry) = self.lock().entries.get_mut(id) {
            entry.stored_bytes = stored_bytes;
        }
    }

    fn complete(&self, id: &str, stored: StoredResult, stored_bytes: u64) {
        let mut state = self.lock();
        match state.entries.get_mut(id) {
            Some(entry) => {
                entry.stored_bytes = stored_bytes;
                entry.stored = Some(Arc::new(stored));
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
            let mut state = self.lock();
            match state.entries.get(id) {
                Some(entry) if entry.stored.is_some() => state.entries.remove(id),
                _ => None,
            }
        };
        removed.map(|entry| entry.remove_file()).is_some()
    }

    fn readable(&self, id: &str) -> Result<(SchemaRef, Arc<StoredResult>), String> {
        let state = self.lock();
        state
            .entries
            .get(id)
            .and_then(|entry| Some((entry.schema.clone(), entry.stored.clone()?)))
            .ok_or_else(|| missing(id))
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
            let state = self.lock();
            let entry = state.entries.get(id).ok_or_else(|| missing(id))?;
            let stored = entry.stored.clone().ok_or_else(|| missing(id))?;
            let file = match &stored.data {
                StoredData::File(path) => Some(File::open(path).map_err(|_| missing(id))?),
                StoredData::Memory(_) => None,
            };
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

    /// One row per readable result, newest first.
    pub(crate) fn list(&self) -> RecordBatch {
        let state = self.lock();
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
    format!("SQLNB_RESULT_MISSING: result '{id}' does not exist or was evicted")
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
    IpcWriteOptions::default()
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

struct CountingWriter<W> {
    inner: W,
    bytes: u64,
}

impl<W: Write> Write for CountingWriter<W> {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        let written = self.inner.write(buffer)?;
        self.bytes += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

struct SpillFile {
    writer: FileWriter<CountingWriter<BufWriter<File>>>,
    temporary: PathBuf,
    path: PathBuf,
}

impl SpillFile {
    fn create(directory: &Path, id: &str, schema: &Schema) -> Result<Self, String> {
        let temporary = directory.join(format!("{id}.arrow.tmp"));
        let file = private_new_file(&temporary)
            .map_err(|error| format!("failed to create {}: {error}", temporary.display()))?;
        let output = CountingWriter {
            inner: BufWriter::with_capacity(FILE_BUFFER_BYTES, file),
            bytes: 0,
        };
        match FileWriter::try_new_with_options(output, schema, write_options()) {
            Ok(writer) => Ok(Self {
                writer,
                temporary,
                path: directory.join(format!("{id}.arrow")),
            }),
            Err(error) => {
                let _ = fs::remove_file(&temporary);
                Err(error.to_string())
            }
        }
    }

    fn bytes(&self) -> u64 {
        self.writer.get_ref().bytes
    }

    /// Writes the footer and publishes the file under its final name.
    fn finish(self) -> Result<(PathBuf, u64), String> {
        let temporary = self.temporary.clone();
        let result = (|| {
            let output = self.writer.into_inner().map_err(|error| error.to_string())?;
            let bytes = output.bytes;
            drop(output.inner.into_inner().map_err(|error| error.to_string())?);
            fs::rename(&self.temporary, &self.path).map_err(|error| error.to_string())?;
            Ok((self.path, bytes))
        })();
        if result.is_err() {
            let _ = fs::remove_file(temporary);
        }
        result
    }

    fn discard(self) {
        let temporary = self.temporary.clone();
        drop(self);
        let _ = fs::remove_file(temporary);
    }
}

/// Stores one statement's rows. Dropping an unfinished writer discards the result.
pub(crate) struct ResultWriter {
    store: Arc<ResultStore>,
    id: String,
    schema: SchemaRef,
    batch_type: DataType,
    limits: ResultLimits,
    started: Instant,
    stored_rows: u64,
    stored_bytes: u64,
    batch_ends: Vec<u64>,
    memory: Vec<RecordBatch>,
    file: Option<SpillFile>,
    limit_reason: Option<LimitReason>,
    finished: bool,
}

impl ResultWriter {
    /// Imports and stores a batch exported through the C Data Interface.
    fn append_ffi(&mut self, array: FFI_ArrowArray) -> bool {
        if self.limit_reason.is_some() {
            return false;
        }
        let batch = unsafe { from_ffi_and_data_type(array, self.batch_type.clone()) }.and_then(|data| {
            RecordBatch::try_new(self.schema.clone(), StructArray::from(data).into_parts().1)
        });
        match batch {
            Ok(batch) => self.append(batch),
            Err(_) => {
                self.stop(LimitReason::Io);
                false
            }
        }
    }

    /// Stores a batch, or the prefix of it that the limits allow. Returns whether
    /// later batches can still be stored.
    pub(crate) fn append(&mut self, batch: RecordBatch) -> bool {
        if self.limit_reason.is_some() {
            return false;
        }
        let rows = batch.num_rows() as u64;
        if rows == 0 {
            return true;
        }
        let bytes = batch_bytes(&batch);
        let (granted, reason) = self.store.reserve(&self.id, self.stored_bytes, bytes, &self.limits);
        let (batch, rows, bytes) = if granted < bytes {
            // Byte sizes are proportional enough to keep the prefix that fits.
            let fitting = (u128::from(rows) * u128::from(granted) / u128::from(bytes.max(1))) as u64;
            (batch.slice(0, fitting as usize), fitting, granted)
        } else {
            (batch, rows, bytes)
        };
        if rows > 0 {
            if let Err(error) = self.store_batch(batch, rows, bytes) {
                self.stop(error);
                return false;
            }
        }
        match reason {
            Some(reason) => {
                self.stop(reason);
                false
            }
            None => true,
        }
    }

    fn store_batch(&mut self, batch: RecordBatch, rows: u64, bytes: u64) -> Result<(), LimitReason> {
        if self.file.is_none()
            && (self.stored_rows + rows > self.limits.head_rows
                || self.stored_bytes + bytes > MEMORY_RESULT_BYTES)
        {
            self.spill()?;
        }
        match &mut self.file {
            Some(file) => {
                file.writer.write(&batch).map_err(|_| LimitReason::Io)?;
                self.stored_bytes = file.bytes();
            }
            None => {
                self.memory.push(batch);
                self.stored_bytes += bytes;
            }
        }
        self.stored_rows += rows;
        self.batch_ends.push(self.stored_rows);
        self.store.record_stored_bytes(&self.id, self.stored_bytes);
        Ok(())
    }

    /// Moves the batches kept in memory into a new file. On failure the
    /// batches stay in memory as the stored prefix.
    fn spill(&mut self) -> Result<(), LimitReason> {
        let directory = self.store.session_directory().map_err(|_| LimitReason::Io)?;
        let mut file = SpillFile::create(&directory, &self.id, &self.schema).map_err(|_| LimitReason::Io)?;
        for batch in &self.memory {
            if file.writer.write(batch).is_err() {
                file.discard();
                return Err(LimitReason::Io);
            }
        }
        self.memory.clear();
        self.stored_bytes = file.bytes();
        self.file = Some(file);
        Ok(())
    }

    /// Stops storing; the rows stored so far remain the result's prefix.
    pub(crate) fn stop(&mut self, reason: LimitReason) {
        self.limit_reason.get_or_insert(reason);
    }

    /// Makes the result readable. `rows` is the number of rows the statement
    /// produced, or only those it read when it stopped early (`rows_exact` false).
    pub(crate) fn finish(mut self, rows: u64, rows_exact: bool) {
        self.finished = true;
        let elapsed_ms = self.started.elapsed().as_millis() as u64;
        let data = match self.file.take() {
            None => StoredData::Memory(std::mem::take(&mut self.memory)),
            Some(file) => match file.finish() {
                Ok((path, bytes)) => {
                    self.stored_bytes = bytes;
                    StoredData::File(path)
                }
                Err(_) => {
                    self.stored_rows = 0;
                    self.stored_bytes = 0;
                    self.batch_ends.clear();
                    self.limit_reason = Some(LimitReason::Io);
                    StoredData::Memory(Vec::new())
                }
            },
        };
        let stored = StoredResult {
            rows,
            rows_exact,
            stored_rows: self.stored_rows,
            limit_reason: self.limit_reason,
            elapsed_ms,
            batch_ends: std::mem::take(&mut self.batch_ends),
            data,
        };
        self.store.complete(&self.id, stored, self.stored_bytes);
    }
}

impl Drop for ResultWriter {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        if let Some(file) = self.file.take() {
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
    error_out: *mut *mut c_char,
) -> *mut ResultWriter {
    let mut writer = None;
    exported(error_out, || {
        let schema = unsafe { import_schema(schema) }?;
        let limits = ResultLimits {
            head_rows,
            max_result_bytes,
            max_cache_bytes,
        };
        writer = Some(STORE.begin(unsafe { text(id, "id") }?, unsafe { text(meta, "meta") }?, schema, limits)?);
        Ok(())
    });
    writer.map_or(std::ptr::null_mut(), |writer| Box::into_raw(Box::new(writer)))
}

#[no_mangle]
pub unsafe extern "C" fn sidequery_result_append(
    writer: *mut ResultWriter,
    batch: *mut FFI_ArrowArray,
) -> bool {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let writer = unsafe { &mut *writer };
        writer.append_ffi(unsafe { FFI_ArrowArray::from_raw(batch) })
    }))
    .unwrap_or(false)
}

#[no_mangle]
pub unsafe extern "C" fn sidequery_result_stop(writer: *mut ResultWriter) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        unsafe { &mut *writer }.stop(LimitReason::Io)
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
