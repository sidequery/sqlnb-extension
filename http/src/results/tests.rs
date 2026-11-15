use super::*;
use arrow_array::cast::AsArray;
use arrow_array::types::Int64Type;

use std::sync::atomic::AtomicI64;

const UNLIMITED: u64 = u64::MAX;
const GIB: u64 = 1024 * 1024 * 1024;
const HOUR_MS: i64 = 60 * 60 * 1000;

/// The time and the disk a store sees, set by the test.
#[derive(Clone)]
struct Environment {
    now_ms: Arc<AtomicI64>,
    /// Free bytes on the cache volume; `u64::MAX` for a volume that does not say.
    free_bytes: Arc<AtomicU64>,
    volume_bytes: Arc<AtomicU64>,
}

impl Environment {
    fn advance(&self, milliseconds: i64) {
        self.now_ms.fetch_add(milliseconds, Ordering::Relaxed);
    }
}

/// A store with a clock that stands still and a volume with room, so that
/// neither the machine's clock nor its disk decides a test.
fn open_store_in(directory: Result<PathBuf, String>) -> (Arc<ResultStore>, Environment) {
    let environment = Environment {
        now_ms: Arc::new(AtomicI64::new(1_700_000_000_000)),
        free_bytes: Arc::new(AtomicU64::new(512 * GIB)),
        volume_bytes: Arc::new(AtomicU64::new(1024 * GIB)),
    };
    let (clock, disk) = (environment.clone(), environment.clone());
    let store = Arc::new(ResultStore::with_environment(
        directory,
        Box::new(move || clock.now_ms.load(Ordering::Relaxed)),
        Box::new(move |_| {
            let available = disk.free_bytes.load(Ordering::Relaxed);
            (available != u64::MAX)
                .then(|| DiskSpace { available, total: disk.volume_bytes.load(Ordering::Relaxed) })
        }),
    ));
    store.retain();
    (store, environment)
}

fn open_store(directory: &Path) -> Arc<ResultStore> {
    open_store_in(Ok(directory.to_path_buf())).0
}

fn schema() -> Schema {
    Schema::new(vec![
        Field::new("value", DataType::Int64, true),
        Field::new("label", DataType::Utf8, true),
    ])
}

fn batch(start: i64, rows: i64) -> RecordBatch {
    RecordBatch::try_new(
        Arc::new(schema()),
        vec![
            Arc::new(Int64Array::from_iter_values(start..start + rows)),
            Arc::new(StringArray::from_iter_values(
                (start..start + rows).map(|value| format!("row-{value}")),
            )),
        ],
    )
    .unwrap()
}

fn limits(head_rows: u64) -> ResultLimits {
    ResultLimits {
        head_rows,
        max_result_bytes: UNLIMITED,
        max_cache_bytes: UNLIMITED,
        ttl_ms: None,
    }
}

fn kept_for(head_rows: u64, ttl_ms: i64) -> ResultLimits {
    ResultLimits { ttl_ms: Some(ttl_ms as u64), ..limits(head_rows) }
}

fn id(number: u8) -> String {
    format!("{number:032x}")
}

/// Stores batches of the given sizes, numbering rows from 0.
fn store_result(store: &Arc<ResultStore>, id: &str, sizes: &[i64], limits: ResultLimits) {
    let writer = store.begin(id, "meta", schema(), limits).unwrap();
    let mut start = 0;
    for (position, &size) in sizes.iter().enumerate() {
        writer.append(batch(start, size), (0, position as u64));
        start += size;
    }
    writer.finish(start as u64, true);
}

/// The scanned values of the first scanned column, and the row numbers when requested.
fn scan_values(
    store: &ResultStore,
    id: &str,
    first_row: u64,
    row_count: u64,
    row_number: Option<&str>,
    columns: &[usize],
) -> Vec<Vec<i64>> {
    let scan = store.scan(id, first_row, row_count, row_number, columns).unwrap();
    let mut values = vec![Vec::new(); columns.len()];
    for batch in scan {
        let batch = batch.unwrap();
        for (column, values) in values.iter_mut().enumerate() {
            let array = batch.column(column);
            if let Some(int64) = array.as_primitive_opt::<Int64Type>() {
                values.extend(int64.values().iter().copied());
            } else {
                values.extend(array.as_string::<i32>().iter().map(|label| {
                    label.unwrap().trim_start_matches("row-").parse::<i64>().unwrap()
                }));
            }
        }
    }
    values
}

fn listed(store: &ResultStore) -> Vec<(String, i64, i64, bool, Option<String>, bool)> {
    let list = store.list();
    let ids = list.column(0).as_string::<i32>();
    let rows = list.column(1).as_primitive::<Int64Type>();
    let stored_rows = list.column(2).as_primitive::<Int64Type>();
    let in_memory = list.column(4).as_boolean();
    let reasons = list.column(5).as_string::<i32>();
    let rows_exact = list.column(9).as_boolean();
    (0..list.num_rows())
        .map(|row| {
            (
                ids.value(row).to_string(),
                rows.value(row),
                stored_rows.value(row),
                in_memory.value(row),
                reasons.is_valid(row).then(|| reasons.value(row).to_string()),
                rows_exact.value(row),
            )
        })
        .collect()
}

fn listed_ids(store: &ResultStore) -> Vec<String> {
    listed(store).into_iter().map(|result| result.0).collect()
}

/// When a listed result was last used and when it expires.
fn usage(store: &ResultStore, id: &str) -> (i64, Option<i64>) {
    let list = store.list();
    let row = list.column(0).as_string::<i32>().iter().position(|listed| listed == Some(id)).unwrap();
    let expires = list.column(11).as_primitive::<Int64Type>();
    (
        list.column(10).as_primitive::<Int64Type>().value(row),
        expires.is_valid(row).then(|| expires.value(row)),
    )
}

fn is_missing<T>(result: Result<T, String>) -> bool {
    result.err().is_some_and(|error| error.starts_with("SQLNB_RESULT_MISSING:"))
}

fn session_files(directory: &Path) -> Vec<String> {
    let mut files = Vec::new();
    for session in fs::read_dir(directory).into_iter().flatten().flatten() {
        for file in fs::read_dir(session.path()).unwrap().flatten() {
            files.push(file.file_name().to_string_lossy().into_owned());
        }
    }
    files.sort();
    files
}

#[test]
fn small_results_stay_in_memory_and_large_ones_spill_to_a_file() {
    let directory = tempfile::tempdir().unwrap();
    let store = open_store(directory.path());
    store_result(&store, &id(1), &[4, 4], limits(8));
    assert!(session_files(directory.path()).is_empty());
    store_result(&store, &id(2), &[4, 4, 1], limits(8));
    assert_eq!(session_files(directory.path()), [format!("{}.arrow", id(2)), SESSION_LOCK.to_string()]);
    assert_eq!(
        listed(&store),
        [
            (id(2), 9, 9, false, None, true),
            (id(1), 8, 8, true, None, true),
        ]
    );
    for result in [id(1), id(2)] {
        let values = scan_values(&store, &result, 0, UNLIMITED, None, &[0, 1]);
        let expected: Vec<i64> = (0..values[0].len() as i64).collect();
        assert_eq!(values, [expected.clone(), expected]);
    }
}

#[test]
fn windows_cross_batch_boundaries_with_projection_and_row_numbers() {
    let directory = tempfile::tempdir().unwrap();
    let store = open_store(directory.path());
    for (result, head_rows) in [(id(1), 100), (id(2), 0)] {
        store_result(&store, &result, &[3, 5, 4], limits(head_rows));
        assert_eq!(
            scan_values(&store, &result, 2, 7, Some("position"), &[2, 1]),
            [(2..9).collect::<Vec<_>>(), (2..9).collect()]
        );
        assert_eq!(scan_values(&store, &result, 8, UNLIMITED, None, &[0])[0], (8..12).collect::<Vec<_>>());
        assert_eq!(scan_values(&store, &result, 3, 5, None, &[0])[0], (3..8).collect::<Vec<_>>());
        assert!(scan_values(&store, &result, 12, UNLIMITED, None, &[0])[0].is_empty());
        assert!(scan_values(&store, &result, 0, 0, None, &[0])[0].is_empty());
    }
}

#[test]
fn row_number_must_not_collide_with_a_result_column() {
    let directory = tempfile::tempdir().unwrap();
    let store = open_store(directory.path());
    store_result(&store, &id(1), &[2], limits(10));
    assert!(store.scan_schema(&id(1), Some("VALUE")).is_err());
    assert!(store.scan_schema(&id(1), Some("")).is_err());
    let schema = store.scan_schema(&id(1), Some("row_index")).unwrap();
    assert_eq!(schema.field(2).name(), "row_index");
    assert_eq!(schema.field(2).data_type(), &DataType::Int64);
}

#[test]
fn ids_are_validated_and_unique() {
    assert!(valid_id("0123456789abcdef0123456789abcdef"));
    for invalid in ["", "0123456789ABCDEF0123456789abcdef", "0123456789abcdef0123456789abcde", "../123456789abcdef0123456789abcd", "0123456789abcdef0123456789abcdeg"] {
        assert!(!valid_id(invalid), "{invalid}");
    }
    let directory = tempfile::tempdir().unwrap();
    let store = open_store(directory.path());
    assert!(store.begin("not-an-id", "", schema(), limits(1)).is_err());
    assert!(store.check(&id(1), &"x".repeat(MAX_META_BYTES + 1), &schema()).is_err());
    let writer = store.begin(&id(1), "", schema(), limits(1)).unwrap();
    assert!(store.check(&id(1), "", &schema()).unwrap_err().starts_with("SQLNB_RESULT_EXISTS:"));
    assert!(store.begin(&id(1), "", schema(), limits(1)).is_err());
    writer.finish(0, true);
    assert!(store.check(&id(1), "", &schema()).is_err());
    assert!(store.scan_schema(&id(9), None).unwrap_err().starts_with("SQLNB_RESULT_MISSING:"));
}

#[test]
fn unfinished_results_are_neither_listed_nor_readable_and_abort_leaves_nothing() {
    let directory = tempfile::tempdir().unwrap();
    let store = open_store(directory.path());
    let writer = store.begin(&id(1), "", schema(), limits(2)).unwrap();
    assert!(writer.append(batch(0, 5), (0, 0)));
    assert_eq!(session_files(directory.path()), [format!("{}.arrow.tmp", id(1)), SESSION_LOCK.to_string()]);
    assert!(listed(&store).is_empty());
    assert!(store.scan(&id(1), 0, UNLIMITED, None, &[0]).is_err());
    assert!(!store.drop_result(&id(1)));
    drop(writer);
    assert_eq!(session_files(directory.path()), [SESSION_LOCK]);
    store_result(&store, &id(1), &[1], limits(2));
    assert_eq!(listed(&store).len(), 1);
}

#[test]
fn the_result_limit_keeps_a_prefix_and_counts_every_row() {
    let directory = tempfile::tempdir().unwrap();
    let store = open_store(directory.path());
    let batch_size = batch_bytes(&batch(0, 100));
    let writer = store
        .begin(
            &id(1),
            "",
            schema(),
            ResultLimits {
                head_rows: 1000,
                max_result_bytes: batch_size + batch_size / 2,
                max_cache_bytes: UNLIMITED,
                ttl_ms: None,
            },
        )
        .unwrap();
    assert!(writer.append(batch(0, 100), (0, 0)));
    assert!(!writer.append(batch(100, 100), (0, 1)));
    assert!(!writer.append(batch(200, 100), (0, 2)));
    writer.finish(300, true);
    assert_eq!(listed(&store), [(id(1), 300, 100, true, Some("result-bytes".into()), true)]);
    assert_eq!(scan_values(&store, &id(1), 0, UNLIMITED, None, &[0])[0], (0..100).collect::<Vec<_>>());
}

#[test]
fn the_cache_limit_evicts_the_oldest_other_results() {
    let directory = tempfile::tempdir().unwrap();
    let store = open_store(directory.path());
    let batch_size = batch_bytes(&batch(0, 100));
    let cache = ResultLimits {
        head_rows: 1000,
        max_result_bytes: UNLIMITED,
        max_cache_bytes: batch_size * 2,
        ttl_ms: None,
    };
    store_result(&store, &id(1), &[100], cache);
    store_result(&store, &id(2), &[100], cache);
    store_result(&store, &id(3), &[100], cache);
    let remaining: Vec<_> = listed(&store).into_iter().map(|result| result.0).collect();
    assert_eq!(remaining, [id(3), id(2)]);
    assert!(store.scan_schema(&id(1), None).unwrap_err().starts_with("SQLNB_RESULT_MISSING:"));

    // An unfinished result cannot be evicted; the newer one stops storing instead.
    let writer = store.begin(&id(4), "", schema(), cache).unwrap();
    assert!(writer.append(batch(0, 100), (0, 0)));
    store_result(&store, &id(5), &[100, 100], cache);
    writer.finish(100, true);
    assert_eq!(
        listed(&store),
        [
            (id(5), 200, 100, true, Some("cache-bytes".into()), true),
            (id(4), 100, 100, true, None, true),
        ]
    );
}

#[test]
fn stopping_early_reports_a_lower_bound() {
    let directory = tempfile::tempdir().unwrap();
    let store = open_store(directory.path());
    let writer = store.begin(&id(1), "", schema(), limits(10)).unwrap();
    writer.append(batch(0, 5), (0, 0));
    writer.missing((0, 1), LimitReason::Io);
    assert!(!writer.append(batch(5, 5), (0, 1)));
    writer.finish(10, false);
    assert_eq!(listed(&store), [(id(1), 10, 5, true, Some("io".into()), false)]);
}

#[test]
fn an_unavailable_cache_directory_keeps_the_memory_prefix() {
    let store = open_store_in(Err("no cache root".into())).0;
    let writer = store.begin(&id(1), "", schema(), limits(4)).unwrap();
    assert!(writer.append(batch(0, 4), (0, 0)));
    assert!(!writer.append(batch(4, 4), (0, 1)));
    writer.finish(8, true);
    assert_eq!(listed(&store), [(id(1), 8, 4, true, Some("io".into()), true)]);
    assert_eq!(scan_values(&store, &id(1), 0, UNLIMITED, None, &[0])[0], (0..4).collect::<Vec<_>>());
}

#[test]
fn dropping_keeps_open_scans_readable() {
    let directory = tempfile::tempdir().unwrap();
    let store = open_store(directory.path());
    store_result(&store, &id(1), &[3, 3], limits(0));
    let scan = store.scan(&id(1), 0, UNLIMITED, None, &[0]).unwrap();
    assert!(store.drop_result(&id(1)));
    assert!(!store.drop_result(&id(1)));
    assert_eq!(session_files(directory.path()), [SESSION_LOCK]);
    assert_eq!(scan.map(|batch| batch.unwrap().num_rows()).sum::<usize>(), 6);
    assert!(store.scan(&id(1), 0, UNLIMITED, None, &[0]).is_err());
}

#[test]
fn closing_the_last_database_removes_the_session() {
    let directory = tempfile::tempdir().unwrap();
    let store = open_store(directory.path());
    store.retain();
    store_result(&store, &id(1), &[3], limits(0));
    store.release();
    assert_eq!(listed(&store).len(), 1);
    store.release();
    assert!(listed(&store).is_empty());
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);
}

#[test]
fn stale_sessions_are_removed_when_a_session_starts() {
    let directory = tempfile::tempdir().unwrap();
    let make_session = |name: &str, age: Duration| {
        let session = directory.path().join(name);
        fs::create_dir(&session).unwrap();
        fs::write(session.join("1.arrow"), b"data").unwrap();
        let lock = File::options().create(true).truncate(true).write(true).open(session.join(SESSION_LOCK)).unwrap();
        lock.set_modified(SystemTime::now() - age).unwrap();
        session
    };
    let old = SESSION_CLAIM_GRACE * 2;
    let abandoned = make_session("abandoned", old);
    let live = make_session("live", old);
    let live_lock = open_lock_file(&live.join(SESSION_LOCK)).unwrap();
    assert!(try_lock_file(&live_lock, LOCK_EX, "test session").unwrap());
    let starting = make_session("starting", Duration::ZERO);

    let store = open_store(directory.path());
    store_result(&store, &id(1), &[2], limits(0));
    assert!(!abandoned.exists());
    assert!(live.join("1.arrow").exists());
    assert!(starting.join("1.arrow").exists());
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 3);
}

#[test]
fn batches_arrive_through_the_c_data_interface() {
    let directory = tempfile::tempdir().unwrap();
    let store = open_store(directory.path());
    let writer = store.begin(&id(1), "", schema(), limits(0)).unwrap();
    for start in [0, 4] {
        let exported = StructArray::from(batch(start, 4)).into_data();
        let (array, _) = arrow_array::ffi::to_ffi(&exported).unwrap();
        assert!(writer.append_ffi(array, (0, start as u64)));
    }
    writer.finish(8, true);
    let values = scan_values(&store, &id(1), 0, UNLIMITED, Some("n"), &[1, 2]);
    assert_eq!(values, [(0..8).collect::<Vec<_>>(), (0..8).collect()]);
}

#[test]
fn batches_stored_out_of_order_are_read_in_key_order() {
    let directory = tempfile::tempdir().unwrap();
    let store = open_store(directory.path());
    // In memory, then in a file that the third batch starts.
    for (result, head_rows) in [(id(1), 100), (id(2), 8)] {
        let writer = store.begin(&result, "", schema(), limits(head_rows)).unwrap();
        for (key, start, rows) in [((2, 0), 9, 3), ((1, 1), 6, 3), ((0, 0), 0, 4), ((1, 0), 4, 2)] {
            assert!(writer.append(batch(start, rows), key));
        }
        writer.finish(12, true);
        let values = scan_values(&store, &result, 0, UNLIMITED, Some("n"), &[0, 2]);
        assert_eq!(values, [(0..12).collect::<Vec<_>>(), (0..12).collect()]);
        assert_eq!(scan_values(&store, &result, 5, 5, None, &[1])[0], (5..10).collect::<Vec<_>>());
    }
    assert_eq!(listed(&store).iter().map(|result| result.3).collect::<Vec<_>>(), [false, true]);
    // Another reader of the file sees the same order: the footer carries it.
    let path = fs::read_dir(directory.path()).unwrap().flatten().next().unwrap().path().join(format!("{}.arrow", id(2)));
    let reader = FileReader::try_new(File::open(path).unwrap(), None).unwrap();
    let values: Vec<i64> = reader
        .flat_map(|batch| batch.unwrap().column(0).as_primitive::<Int64Type>().values().to_vec())
        .collect();
    assert_eq!(values, (0..12).collect::<Vec<_>>());
}

#[test]
fn a_missing_batch_keeps_only_the_batches_before_it() {
    let directory = tempfile::tempdir().unwrap();
    let store = open_store(directory.path());
    let writer = store.begin(&id(1), "", schema(), limits(0)).unwrap();
    assert!(writer.append(batch(0, 4), (0, 0)));
    assert!(writer.append(batch(8, 4), (2, 0)));
    writer.missing((1, 0), LimitReason::Io);
    assert!(!writer.append(batch(12, 4), (3, 0)));
    writer.finish(16, true);
    assert_eq!(listed(&store), [(id(1), 16, 4, false, Some("io".into()), true)]);
    assert_eq!(scan_values(&store, &id(1), 0, UNLIMITED, None, &[0])[0], (0..4).collect::<Vec<_>>());
}

#[test]
fn threads_store_batches_at_the_same_time() {
    let directory = tempfile::tempdir().unwrap();
    let store = open_store(directory.path());
    let writer = store.begin(&id(1), "", schema(), limits(0)).unwrap();
    const THREADS: i64 = 8;
    const BATCHES: i64 = 50;
    const ROWS: i64 = 100;
    std::thread::scope(|scope| {
        for thread in 0..THREADS {
            let writer = &writer;
            scope.spawn(move || {
                // Each thread owns every eighth batch, as a thread owns its part of a scan.
                for batch_number in (thread..THREADS * BATCHES).step_by(THREADS as usize) {
                    assert!(writer.append(batch(batch_number * ROWS, ROWS), (batch_number as u64, 0)));
                }
            });
        }
    });
    let total = THREADS * BATCHES * ROWS;
    writer.finish(total as u64, true);
    assert_eq!(scan_values(&store, &id(1), 0, UNLIMITED, None, &[0])[0], (0..total).collect::<Vec<_>>());
}

#[test]
fn a_lower_cache_limit_evicts_the_oldest_results_when_the_next_result_begins() {
    let directory = tempfile::tempdir().unwrap();
    let store = open_store(directory.path());
    for result in 1..=3 {
        store_result(&store, &id(result), &[100], limits(0));
    }
    let result_bytes = store.lock().entries[&id(3)].stored_bytes;
    let lowered = ResultLimits { max_cache_bytes: result_bytes * 2, ..limits(0) };
    let writer = store.begin(&id(4), "", schema(), lowered).unwrap();
    // Before the new result stored anything.
    assert_eq!(listed_ids(&store), [id(3), id(2)]);
    assert_eq!(
        session_files(directory.path()),
        [format!("{}.arrow", id(2)), format!("{}.arrow", id(3)), SESSION_LOCK.to_string()]
    );
    drop(writer);
}

#[test]
fn a_caller_that_names_no_limits_keeps_4_gib_of_a_result_8_gib_in_all_for_24_hours() {
    let directory = tempfile::tempdir().unwrap();
    let store = open_store(directory.path());
    let defaults = ResultLimits::defaults(1000);
    let batch_size = batch_bytes(&batch(0, 100));
    let set_stored_bytes = |result: &str, bytes: u64| {
        store.lock().entries.get_mut(result).unwrap().stored_bytes = bytes;
    };

    store_result(&store, &id(1), &[100], defaults);
    let (last_used, expires) = usage(&store, &id(1));
    assert_eq!(expires, Some(last_used + 24 * HOUR_MS));

    // The cache holds one batch less than 8 GiB: one more batch fits, a second does not.
    set_stored_bytes(&id(1), 8 * GIB - batch_size);
    store_result(&store, &id(2), &[100], defaults);
    assert_eq!(listed_ids(&store), [id(2), id(1)]);
    store_result(&store, &id(3), &[100], defaults);
    assert_eq!(listed_ids(&store), [id(3), id(2)]);

    // A result that holds one batch less than 4 GiB takes one more batch.
    let writer = store.begin(&id(4), "", schema(), defaults).unwrap();
    set_stored_bytes(&id(4), 4 * GIB - batch_size);
    assert!(writer.append(batch(0, 100), (0, 0)));
    assert!(!writer.append(batch(100, 100), (0, 1)));
    writer.finish(200, true);
    assert_eq!(listed(&store)[0], (id(4), 200, 100, true, Some("result-bytes".into()), true));
}

#[test]
fn a_result_expires_after_its_period_without_use() {
    let directory = tempfile::tempdir().unwrap();
    let (store, environment) = open_store_in(Ok(directory.path().to_path_buf()));
    store_result(&store, &id(1), &[3, 3], kept_for(0, HOUR_MS));
    store_result(&store, &id(2), &[3], kept_for(10, HOUR_MS));
    store_result(&store, &id(3), &[3], limits(10));
    let (last_used, expires) = usage(&store, &id(1));
    assert_eq!(expires, Some(last_used + HOUR_MS));
    assert_eq!(usage(&store, &id(3)).1, None);

    environment.advance(HOUR_MS - 1);
    assert_eq!(listed_ids(&store), [id(3), id(2), id(1)]);

    environment.advance(1);
    assert_eq!(listed_ids(&store), [id(3)]);
    assert_eq!(session_files(directory.path()), [SESSION_LOCK]);
    assert!(is_missing(store.scan_schema(&id(1), None)));
    assert!(is_missing(store.scan(&id(2), 0, UNLIMITED, None, &[0]).map(|_| ())));
    assert!(!store.drop_result(&id(1)));
    // The id is free again, as after an eviction.
    store_result(&store, &id(1), &[1], limits(10));

    // A result without a period stays until the cache closes.
    environment.advance(10 * 365 * 24 * HOUR_MS);
    assert_eq!(listed_ids(&store), [id(1), id(3)]);
}

#[test]
fn reading_a_result_starts_its_period_again_and_listing_does_not() {
    let directory = tempfile::tempdir().unwrap();
    let (store, environment) = open_store_in(Ok(directory.path().to_path_buf()));
    store_result(&store, &id(1), &[3, 3], kept_for(0, HOUR_MS));
    store_result(&store, &id(2), &[3, 3], kept_for(0, HOUR_MS));
    store_result(&store, &id(3), &[3, 3], kept_for(0, HOUR_MS));
    let stored_at = usage(&store, &id(1)).0;

    environment.advance(HOUR_MS - 1);
    assert_eq!(scan_values(&store, &id(1), 2, 2, None, &[0])[0], [2, 3]);
    store.scan_schema(&id(2), None).unwrap();
    assert_eq!(usage(&store, &id(1)), (stored_at + HOUR_MS - 1, Some(stored_at + 2 * HOUR_MS - 1)));
    assert_eq!(usage(&store, &id(3)), (stored_at, Some(stored_at + HOUR_MS)));

    // Only the two that were read are still there.
    environment.advance(1);
    assert_eq!(listed_ids(&store), [id(2), id(1)]);
    environment.advance(HOUR_MS - 2);
    assert_eq!(listed_ids(&store), [id(2), id(1)]);
    environment.advance(1);
    assert!(listed_ids(&store).is_empty());
    assert_eq!(session_files(directory.path()), [SESSION_LOCK]);
}

#[test]
fn the_period_starts_when_the_statement_ends() {
    let directory = tempfile::tempdir().unwrap();
    let (store, environment) = open_store_in(Ok(directory.path().to_path_buf()));
    let writer = store.begin(&id(1), "", schema(), kept_for(0, HOUR_MS)).unwrap();
    assert!(writer.append(batch(0, 5), (0, 0)));
    environment.advance(3 * HOUR_MS);
    // A statement that runs longer than the period is not swept while it runs.
    assert!(listed(&store).is_empty());
    assert!(writer.append(batch(5, 5), (0, 1)));
    writer.finish(10, true);
    let finished_at = environment.now_ms.load(Ordering::Relaxed);
    assert_eq!(usage(&store, &id(1)), (finished_at, Some(finished_at + HOUR_MS)));
    assert_eq!(scan_values(&store, &id(1), 0, UNLIMITED, None, &[0])[0], (0..10).collect::<Vec<_>>());
}

#[test]
fn every_use_of_the_cache_removes_the_results_that_expired() {
    type Touch = fn(&Arc<ResultStore>);
    let touches: [(&str, Touch); 6] = [
        ("check", |store| store.check(&id(9), "", &schema()).unwrap()),
        ("begin", |store| drop(store.begin(&id(9), "", schema(), limits(0)).unwrap())),
        ("list", |store| drop(store.list())),
        ("schema", |store| drop(store.scan_schema(&id(2), None).unwrap())),
        ("scan", |store| drop(store.scan(&id(2), 0, UNLIMITED, None, &[0]).unwrap())),
        ("drop", |store| assert!(store.drop_result(&id(2)))),
    ];
    for (name, touch) in touches {
        let directory = tempfile::tempdir().unwrap();
        let (store, environment) = open_store_in(Ok(directory.path().to_path_buf()));
        store_result(&store, &id(1), &[3], kept_for(0, HOUR_MS));
        store_result(&store, &id(2), &[3], limits(0));
        environment.advance(HOUR_MS);
        assert!(session_files(directory.path()).contains(&format!("{}.arrow", id(1))), "{name}");
        touch(&store);
        assert!(!session_files(directory.path()).contains(&format!("{}.arrow", id(1))), "{name}");
        assert!(!store.lock().entries.contains_key(&id(1)), "{name}");
    }
}

#[test]
fn a_scan_that_is_open_when_its_result_expires_reads_to_the_end() {
    let directory = tempfile::tempdir().unwrap();
    let (store, environment) = open_store_in(Ok(directory.path().to_path_buf()));
    store_result(&store, &id(1), &[3, 3], kept_for(0, HOUR_MS));
    let scan = store.scan(&id(1), 0, UNLIMITED, None, &[0]).unwrap();
    environment.advance(HOUR_MS);
    assert!(listed(&store).is_empty());
    assert_eq!(scan.map(|batch| batch.unwrap().num_rows()).sum::<usize>(), 6);
    assert!(is_missing(store.scan(&id(1), 0, UNLIMITED, None, &[0]).map(|_| ())));
}

#[test]
fn nothing_is_written_to_a_volume_below_its_free_space_floor() {
    let directory = tempfile::tempdir().unwrap();
    let (store, environment) = open_store_in(Ok(directory.path().to_path_buf()));
    let two_batches = batch_bytes(&batch(0, 100)) + batch_bytes(&batch(100, 100));

    // One byte short of 5 GiB left once the file held its first two batches.
    environment.free_bytes.store(5 * GIB + two_batches - 1, Ordering::Relaxed);
    let writer = store.begin(&id(1), "", schema(), limits(100)).unwrap();
    assert!(writer.append(batch(0, 100), (0, 0)));
    assert!(!writer.append(batch(100, 100), (0, 1)));
    writer.finish(200, true);
    // The statement goes on; the rows before the floor stay, in memory.
    assert_eq!(listed(&store), [(id(1), 200, 100, true, Some("disk-space".into()), true)]);
    assert_eq!(scan_values(&store, &id(1), 0, UNLIMITED, None, &[0])[0], (0..100).collect::<Vec<_>>());
    assert_eq!(session_files(directory.path()), [SESSION_LOCK]);

    // A result that fits in memory needs no disk.
    environment.free_bytes.store(0, Ordering::Relaxed);
    store_result(&store, &id(2), &[100], limits(100));
    assert_eq!(listed(&store)[0], (id(2), 100, 100, true, None, true));

    // With room again, results are written again.
    environment.free_bytes.store(5 * GIB + two_batches, Ordering::Relaxed);
    store_result(&store, &id(3), &[100, 100], limits(100));
    assert_eq!(listed(&store)[0], (id(3), 200, 200, false, None, true));
}

#[test]
fn a_result_being_written_stops_when_the_volume_runs_low() {
    let directory = tempfile::tempdir().unwrap();
    let (store, environment) = open_store_in(Ok(directory.path().to_path_buf()));
    let writer = store.begin(&id(1), "", schema(), limits(0)).unwrap();
    assert!(writer.append(batch(0, 100), (0, 0)));
    environment.free_bytes.store(GIB, Ordering::Relaxed);
    // The free space is not read for every batch, only every 64 MiB written.
    assert!(writer.append(batch(100, 100), (0, 1)));
    store.unprobed_bytes.store(DISK_PROBE_BYTES, Ordering::Relaxed);
    assert!(!writer.append(batch(200, 100), (0, 2)));

    // Once low, the space is read before every write, also for other results.
    let other = store.begin(&id(2), "", schema(), limits(0)).unwrap();
    assert!(!other.append(batch(0, 100), (0, 0)));
    other.finish(100, true);
    writer.finish(300, true);
    assert_eq!(
        listed(&store),
        [
            (id(2), 100, 0, true, Some("disk-space".into()), true),
            (id(1), 300, 200, false, Some("disk-space".into()), true),
        ]
    );
    assert_eq!(scan_values(&store, &id(1), 0, UNLIMITED, None, &[0])[0], (0..200).collect::<Vec<_>>());
}

#[test]
fn the_floor_of_a_small_volume_is_a_tenth_of_it_and_an_unknown_volume_has_none() {
    let directory = tempfile::tempdir().unwrap();
    let (store, environment) = open_store_in(Ok(directory.path().to_path_buf()));
    environment.volume_bytes.store(20 * GIB, Ordering::Relaxed);
    environment.free_bytes.store(3 * GIB, Ordering::Relaxed);
    store_result(&store, &id(1), &[100], limits(0));
    environment.free_bytes.store(2 * GIB, Ordering::Relaxed);
    store_result(&store, &id(2), &[100], limits(0));
    environment.free_bytes.store(u64::MAX, Ordering::Relaxed);
    store_result(&store, &id(3), &[100], limits(0));
    assert_eq!(
        listed(&store),
        [
            (id(3), 100, 100, false, None, true),
            (id(2), 100, 0, true, Some("disk-space".into()), true),
            (id(1), 100, 100, false, None, true),
        ]
    );
}

#[test]
fn the_volume_of_a_directory_reports_its_space() {
    let directory = tempfile::tempdir().unwrap();
    let space = disk_space(directory.path()).expect("the temporary directory is on a volume");
    assert!(space.available > 0 && space.available <= space.total, "{space:?}");
}
