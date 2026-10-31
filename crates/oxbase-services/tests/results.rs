//! `ResultStore`: memory budget, spill segments, pages across memory and files, the disk cap,
//! paging from another thread while appends continue, and cleanup.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use arrow_array::{Array, Int64Array, RecordBatch, StringArray};
use arrow_schema::{DataType, Field, Schema, SchemaRef};
use oxbase_services::results::{AppendOutcome, ByteBudget, ResultStore, RunDir};
use oxbase_types::ErrorCategory;

fn schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("id", DataType::Int64, false),
        Field::new("name", DataType::Utf8, true),
    ]))
}

fn batch(start: i64, n: i64) -> RecordBatch {
    let ids: Vec<i64> = (start..start + n).collect();
    let names: Vec<Option<String>> = ids
        .iter()
        .map(|i| (i % 7 != 0).then(|| format!("row {i}")))
        .collect();
    RecordBatch::try_new(
        schema(),
        vec![
            Arc::new(Int64Array::from(ids)),
            Arc::new(StringArray::from(names)),
        ],
    )
    .unwrap()
}

fn check(page: &RecordBatch, first: i64) {
    assert_eq!(page.schema(), schema());
    let ids = page
        .column(0)
        .as_any()
        .downcast_ref::<Int64Array>()
        .unwrap();
    let names = page
        .column(1)
        .as_any()
        .downcast_ref::<StringArray>()
        .unwrap();
    for i in 0..page.num_rows() {
        let id = first + i64::try_from(i).unwrap();
        assert_eq!(ids.value(i), id);
        if id % 7 == 0 {
            assert!(names.is_null(i));
        } else {
            assert_eq!(names.value(i), format!("row {id}"));
        }
    }
}

struct Setup {
    _base: tempfile::TempDir,
    dir: Arc<RunDir>,
    memory: Arc<ByteBudget>,
    disk: Arc<ByteBudget>,
}

fn setup(memory: u64, disk: u64) -> Setup {
    let base = tempfile::tempdir().unwrap();
    let dir = Arc::new(RunDir::create(base.path()).unwrap());
    Setup {
        _base: base,
        dir,
        memory: Arc::new(ByteBudget::new(memory)),
        disk: Arc::new(ByteBudget::new(disk)),
    }
}

impl Setup {
    fn store(&self) -> ResultStore {
        ResultStore::new(
            schema(),
            self.dir.clone(),
            self.memory.clone(),
            self.disk.clone(),
        )
    }

    fn files(&self) -> usize {
        std::fs::read_dir(self.dir.path()).unwrap().count()
    }
}

#[test]
fn small_results_stay_in_memory() {
    let s = setup(u64::MAX, u64::MAX);
    let store = s.store();
    assert_eq!(store.page(0, 10).unwrap().num_rows(), 0);
    for i in 0..5 {
        assert_eq!(
            store.append(batch(i * 10, 10)).unwrap(),
            AppendOutcome::Stored
        );
    }
    store.append(batch(50, 0)).unwrap();
    assert_eq!(store.row_count(), 50);
    assert!(!store.is_complete());
    check(&store.page(5, 20).unwrap(), 5);
    check(&store.page(45, 100).unwrap(), 45);
    assert_eq!(store.page(45, 100).unwrap().num_rows(), 5);
    assert_eq!(store.page(50, 10).unwrap().num_rows(), 0);
    assert_eq!(store.page(10, 0).unwrap().num_rows(), 0);
    assert_eq!(store.disk_bytes(), 0);
    assert_eq!(s.files(), 0);
    assert!(store.memory_bytes() > 0);
    assert_eq!(s.memory.used(), store.memory_bytes());
    store.finish(false);
    assert!(store.is_complete() && !store.is_truncated());
    // A finished store takes no more rows.
    let err = store.append(batch(50, 1)).unwrap_err();
    assert_eq!(err.category(), ErrorCategory::Internal);
    drop(store);
    assert_eq!(s.memory.used(), 0);
}

#[test]
fn batches_of_another_schema_are_refused() {
    let s = setup(u64::MAX, u64::MAX);
    let store = s.store();
    let other = RecordBatch::try_new(
        Arc::new(Schema::new(vec![Field::new("x", DataType::Int64, false)])),
        vec![Arc::new(Int64Array::from(vec![1]))],
    )
    .unwrap();
    assert_eq!(
        store.append(other).unwrap_err().category(),
        ErrorCategory::Internal
    );
}

#[test]
fn past_the_budget_batches_spill_and_pages_span_files_and_memory() {
    let one = u64::try_from(batch(0, 100).get_array_memory_size()).unwrap();
    // Room for about three batches in memory.
    let s = setup(one * 3 + one / 2, u64::MAX);
    let store = s.store();
    for i in 0..40 {
        assert_eq!(
            store.append(batch(i * 100, 100)).unwrap(),
            AppendOutcome::Stored
        );
        assert!(
            store.memory_bytes() <= one * 3 + one / 2,
            "memory stays in budget"
        );
        // Every row so far is readable, wherever it lives.
        let rows = (i + 1) * 100;
        check(&store.page(0, 150).unwrap(), 0);
        let tail = (rows - 130).max(0);
        check(
            &store.page(u64::try_from(tail).unwrap(), 1000).unwrap(),
            tail,
        );
    }
    assert_eq!(store.row_count(), 4000);
    assert!(store.disk_bytes() > 0);
    assert_eq!(s.disk.used(), store.disk_bytes());
    assert!(s.files() >= 9, "{} segment files", s.files());
    // Random pages.
    for offset in (0..4000).step_by(173) {
        let page = store.page(offset, 333).unwrap();
        assert_eq!(
            page.num_rows(),
            usize::try_from(333.min(4000 - offset)).unwrap()
        );
        check(&page, i64::try_from(offset).unwrap());
    }
    check(&store.page(0, 4000).unwrap(), 0);
    store.finish(true);
    assert!(store.is_truncated());
    drop(store);
    assert_eq!(s.files(), 0, "segment files go with the store");
    assert_eq!(s.disk.used(), 0);
    assert_eq!(s.memory.used(), 0);
}

#[test]
fn the_disk_cap_stops_storing() {
    let one = u64::try_from(batch(0, 100).get_array_memory_size()).unwrap();
    let s = setup(one * 2, one * 3);
    let store = s.store();
    let mut stored = 0;
    let mut outcome = AppendOutcome::Stored;
    for i in 0..50 {
        outcome = store.append(batch(i * 100, 100)).unwrap();
        if outcome == AppendOutcome::DiskFull {
            break;
        }
        stored += 100;
    }
    assert_eq!(outcome, AppendOutcome::DiskFull);
    assert!(stored >= 100, "{stored}");
    assert_eq!(store.row_count(), u64::try_from(stored).unwrap());
    assert!(s.disk.used() <= s.disk.limit());
    // Nothing more is stored; what is there stays readable.
    assert_eq!(
        store.append(batch(stored, 100)).unwrap(),
        AppendOutcome::DiskFull
    );
    check(&store.page(0, 100_000).unwrap(), 0);
    assert_eq!(
        store.page(0, 100_000).unwrap().num_rows(),
        usize::try_from(stored).unwrap()
    );
}

#[test]
fn pages_from_another_thread_while_appending() {
    let s = setup(64 * 1024, u64::MAX);
    let store = Arc::new(s.store());
    let done = Arc::new(AtomicBool::new(false));
    let reader = {
        let store = store.clone();
        let done = done.clone();
        std::thread::spawn(move || {
            let mut reads = 0;
            loop {
                let finished = done.load(Ordering::Acquire);
                let rows = store.row_count();
                if rows > 0 {
                    let offset = (rows * 7919 + 13) % rows;
                    let page = store.page(offset, 50).unwrap();
                    check(&page, i64::try_from(offset).unwrap());
                    reads += 1;
                }
                if finished {
                    return reads;
                }
            }
        })
    };
    for i in 0..300 {
        store.append(batch(i * 50, 50)).unwrap();
    }
    store.finish(false);
    done.store(true, Ordering::Release);
    assert!(reader.join().unwrap() > 0);
    check(&store.page(0, 15_000).unwrap(), 0);
    assert!(store.disk_bytes() > 0);
}

#[test]
fn ipc_pages_decode() {
    let s = setup(1, u64::MAX);
    let store = s.store();
    store.append(batch(0, 100)).unwrap();
    store.append(batch(100, 100)).unwrap();
    let page = store.page(90, 20).unwrap();
    let bytes = oxbase_services::results::encode_ipc_stream(&page).unwrap();
    let reader =
        arrow_ipc::reader::StreamReader::try_new(std::io::Cursor::new(bytes), None).unwrap();
    let batches: Vec<RecordBatch> = reader.collect::<Result<_, _>>().unwrap();
    assert_eq!(batches.len(), 1);
    check(&batches[0], 90);
}
