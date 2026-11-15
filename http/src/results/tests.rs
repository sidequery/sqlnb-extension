use super::*;
use arrow_array::cast::AsArray;
use arrow_array::types::Int64Type;

const UNLIMITED: u64 = u64::MAX;

fn open_store(directory: &Path) -> Arc<ResultStore> {
    let store = Arc::new(ResultStore::new(Ok(directory.to_path_buf())));
    store.retain();
    store
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
    }
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
    let store = Arc::new(ResultStore::new(Err("no cache root".into())));
    store.retain();
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
