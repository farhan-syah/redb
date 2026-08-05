use redb::{
    Database, Error, ReadableDatabase, ReadableTable, ReadableTableMetadata, SortedTableOptions,
    TableDefinition, TableError,
};

const EMPTY: TableDefinition<u64, u64> = TableDefinition::new("empty");
const FIXED: TableDefinition<u64, u64> = TableDefinition::new("fixed");
const VARIABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("variable");
const TUPLE_LIKE: TableDefinition<[u64; 2], u64> = TableDefinition::new("tuple_like");
const FIRST: TableDefinition<u64, u64> = TableDefinition::new("first");
const SECOND: TableDefinition<u64, u64> = TableDefinition::new("second");

fn create_tempfile() -> tempfile::NamedTempFile {
    if cfg!(target_os = "wasi") {
        tempfile::NamedTempFile::new_in("/tmp").unwrap()
    } else {
        tempfile::NamedTempFile::new().unwrap()
    }
}

#[test]
fn builds_empty_fixed_variable_and_tuple_tables() {
    let file = create_tempfile();
    let db = Database::create(file.path()).unwrap();
    let txn = db
        .begin_write()
        .unwrap()
        .build_sorted_table(EMPTY, |_| Ok(()))
        .unwrap()
        .build_sorted_table(FIXED, |builder| {
            for key in 0..256u64 {
                builder.insert(key, key * 2)?;
            }
            Ok(())
        })
        .unwrap()
        .build_sorted_table(VARIABLE, |builder| {
            builder.insert("a", b"one".as_slice())?;
            builder.insert("alphabet", b"two-two".as_slice())?;
            builder.insert("z", b"three".as_slice())?;
            Ok(())
        })
        .unwrap()
        .build_sorted_table(TUPLE_LIKE, |builder| {
            for major in 0..8u64 {
                for minor in 0..8u64 {
                    builder.insert([major, minor], major * 100 + minor)?;
                }
            }
            Ok(())
        })
        .unwrap();
    txn.commit().unwrap();

    let read = db.begin_read().unwrap();
    assert_eq!(read.open_table(EMPTY).unwrap().len().unwrap(), 0);
    let fixed = read.open_table(FIXED).unwrap();
    assert_eq!(fixed.len().unwrap(), 256);
    assert_eq!(fixed.get(17).unwrap().unwrap().value(), 34);
    let variable = read.open_table(VARIABLE).unwrap();
    assert_eq!(variable.len().unwrap(), 3);
    assert_eq!(
        variable.get("alphabet").unwrap().unwrap().value(),
        b"two-two"
    );
    let tuples = read.open_table(TUPLE_LIKE).unwrap();
    assert_eq!(tuples.len().unwrap(), 64);
    assert_eq!(tuples.get([7, 3]).unwrap().unwrap().value(), 703);
}

#[test]
fn larger_target_pages_commit_and_reopen() {
    const LARGE: TableDefinition<u64, u64> = TableDefinition::new("large_pages");
    let file = create_tempfile();
    let db = Database::create(file.path()).unwrap();
    let txn = db
        .begin_write()
        .unwrap()
        .build_sorted_table_with_options(
            LARGE,
            SortedTableOptions::default().with_target_page_size(64 * 1024),
            |builder| {
                for key in 0..50_000u64 {
                    builder.insert(key, key)?;
                }
                Ok(())
            },
        )
        .unwrap();
    txn.commit().unwrap();
    drop(db);

    let mut db = Database::open(file.path()).unwrap();
    db.check_integrity().unwrap();
    let read = db.begin_read().unwrap();
    let table = read.open_table(LARGE).unwrap();
    assert_eq!(table.len().unwrap(), 50_000);
    assert_eq!(table.get(49_999).unwrap().unwrap().value(), 49_999);
}

#[test]
fn trailing_single_child_is_rebalanced_before_finish() {
    const BOUNDARY: TableDefinition<u64, u64> = TableDefinition::new("boundary");
    let file = create_tempfile();
    let db = Database::create(file.path()).unwrap();
    let txn = db
        .begin_write()
        .unwrap()
        .build_sorted_table(BOUNDARY, |builder| {
            for key in 0..32_641u64 {
                builder.insert(key, key)?;
            }
            Ok(())
        })
        .unwrap();
    txn.commit().unwrap();
    let read = db.begin_read().unwrap();
    let table = read.open_table(BOUNDARY).unwrap();
    assert_eq!(table.len().unwrap(), 32_641);
    assert_eq!(table.get(32_640).unwrap().unwrap().value(), 32_640);
}

#[test]
fn builds_multilevel_table_and_reopens_with_integrity() {
    let file = create_tempfile();
    let mut db = Database::create(file.path()).unwrap();
    let txn = db
        .begin_write()
        .unwrap()
        .build_sorted_table(FIXED, |builder| {
            for key in 0..50_000u64 {
                builder.insert(key, key ^ 0x55aa)?;
            }
            Ok(())
        })
        .unwrap();
    txn.commit().unwrap();
    assert!(db.check_integrity().unwrap());
    drop(db);

    let mut reopened = Database::open(file.path()).unwrap();
    assert!(reopened.check_integrity().unwrap());
    let read = reopened.begin_read().unwrap();
    let table = read.open_table(FIXED).unwrap();
    assert_eq!(table.len().unwrap(), 50_000);
    assert_eq!(table.get(0).unwrap().unwrap().value(), 0x55aa);
    assert_eq!(table.get(49_999).unwrap().unwrap().value(), 49_999 ^ 0x55aa);
    let mut expected = 0u64;
    for entry in table.iter().unwrap() {
        let (key, value) = entry.unwrap();
        assert_eq!(key.value(), expected);
        assert_eq!(value.value(), expected ^ 0x55aa);
        expected += 1;
    }
    assert_eq!(expected, 50_000);
}

#[test]
fn nonmonotonic_input_aborts_the_consumed_transaction() {
    let file = create_tempfile();
    let db = Database::create(file.path()).unwrap();
    let result = db
        .begin_write()
        .unwrap()
        .build_sorted_table(FIXED, |builder| {
            builder.insert(2, 20)?;
            builder.insert(1, 10)?;
            Ok(())
        });
    assert!(matches!(result, Err(Error::SortedTableKeyOrder)));

    let read = db.begin_read().unwrap();
    assert!(matches!(
        read.open_table(FIXED),
        Err(TableError::TableDoesNotExist(_))
    ));
}

#[test]
fn ignored_insert_error_still_aborts_the_consumed_transaction() {
    let file = create_tempfile();
    let db = Database::create(file.path()).unwrap();
    let result = db
        .begin_write()
        .unwrap()
        .build_sorted_table(FIXED, |builder| {
            builder.insert(2, 20)?;
            let _ = builder.insert(1, 10);
            Ok(())
        });
    assert!(matches!(result, Err(Error::SortedTableBuilderFailed)));

    let read = db.begin_read().unwrap();
    assert!(matches!(
        read.open_table(FIXED),
        Err(TableError::TableDoesNotExist(_))
    ));
}

#[test]
fn producer_error_aborts_the_consumed_transaction() {
    let file = create_tempfile();
    let db = Database::create(file.path()).unwrap();
    let result = db
        .begin_write()
        .unwrap()
        .build_sorted_table(FIXED, |builder| {
            builder.insert(1, 10)?;
            Err(Error::Corrupted("injected producer failure".into()))
        });
    assert!(matches!(result, Err(Error::Corrupted(message)) if message.contains("injected")));

    let read = db.begin_read().unwrap();
    assert!(matches!(
        read.open_table(FIXED),
        Err(TableError::TableDoesNotExist(_))
    ));
}

#[test]
fn nonempty_target_is_rejected_without_changing_existing_data() {
    let file = create_tempfile();
    let db = Database::create(file.path()).unwrap();
    let txn = db.begin_write().unwrap();
    {
        let mut table = txn.open_table(FIXED).unwrap();
        table.insert(7, 77).unwrap();
    }
    txn.commit().unwrap();

    let result = db
        .begin_write()
        .unwrap()
        .build_sorted_table(FIXED, |_| Ok(()));
    assert!(matches!(result, Err(Error::SortedTableNotEmpty(name)) if name == "fixed"));

    let read = db.begin_read().unwrap();
    let table = read.open_table(FIXED).unwrap();
    assert_eq!(table.len().unwrap(), 1);
    assert_eq!(table.get(7).unwrap().unwrap().value(), 77);
}

#[test]
fn sequential_builds_publish_both_tables_in_one_commit() {
    let file = create_tempfile();
    let db = Database::create(file.path()).unwrap();
    let txn = db
        .begin_write()
        .unwrap()
        .build_sorted_table(FIRST, |builder| {
            for key in 0..1_000u64 {
                builder.insert(key, key + 1)?;
            }
            Ok(())
        })
        .unwrap()
        .build_sorted_table(SECOND, |builder| {
            for key in 1_000..2_000u64 {
                builder.insert(key, key + 1)?;
            }
            Ok(())
        })
        .unwrap();
    txn.commit().unwrap();

    let read = db.begin_read().unwrap();
    assert_eq!(read.open_table(FIRST).unwrap().len().unwrap(), 1_000);
    assert_eq!(read.open_table(SECOND).unwrap().len().unwrap(), 1_000);
    assert_eq!(
        read.open_table(SECOND)
            .unwrap()
            .get(1_999)
            .unwrap()
            .unwrap()
            .value(),
        2_000
    );
}
