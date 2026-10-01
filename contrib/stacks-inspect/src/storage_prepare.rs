//! Canonical offline preparation before opening validation handles.

use std::path::Path;

/// Prepare every applicable MARF in a disposable validation chainstate.
pub fn prepare_for_validation(database_root: &Path) -> stacks_storage_migrate::Result<()> {
    stacks_storage_migrate::prepare_chainstate_in_place(database_root)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clarity::vm::database::SqliteConnection;
    use rusqlite::{Connection, params};
    use stacks_common::types::chainstate::StacksBlockId;
    use stackslib::chainstate::stacks::index::marf::{MARF, MARFOpenOpts, MarfConnection};
    use stackslib::chainstate::stacks::index::record::NodeRecordFormat;
    use stackslib::chainstate::stacks::index::{ClarityMarfTrieId, MARFValue};
    use std::fs;

    /// A populated legacy chainstate retains its root through preparation and a second startup.
    #[test]
    fn prepares_legacy_chainstate_and_reuses_published_formats() {
        let temporary = tempfile::tempdir().unwrap();
        let vm = temporary.path().join("chainstate/vm");
        fs::create_dir_all(vm.join("clarity")).unwrap();
        let mut opts = MARFOpenOpts::default();
        opts.external_blobs = true;
        let clarity_path = vm.join("clarity/marf.sqlite");
        let mut clarity =
            MARF::<StacksBlockId>::from_path(clarity_path.to_str().unwrap(), opts.clone()).unwrap();
        SqliteConnection::initialize_conn(clarity.sqlite_conn()).unwrap();
        let block = StacksBlockId([7; 32]);
        let value = "prepared-value";
        let hash = MARFValue::from_value(value);
        let mut tx = clarity.begin_tx().unwrap();
        tx.begin(&StacksBlockId::sentinel(), &block).unwrap();
        tx.sqlite_tx()
            .execute(
                "INSERT INTO data_table(key,value) VALUES (?1,?2)",
                params![hash.to_hex(), value],
            )
            .unwrap();
        tx.insert_batch(&["prepared-key".into()], &[hash]).unwrap();
        tx.seal().unwrap();
        tx.commit().unwrap();
        let expected_root = clarity.get_root_hash_at(&block).unwrap();
        drop(clarity);
        let headers =
            MARF::<StacksBlockId>::from_path(vm.join("index.sqlite").to_str().unwrap(), opts)
                .unwrap();
        drop(headers);

        prepare_for_validation(temporary.path()).unwrap();
        assert_eq!(
            NodeRecordFormat::from_database(&Connection::open(&clarity_path).unwrap()).unwrap(),
            NodeRecordFormat::Optimized
        );
        let mut reopen_opts = MARFOpenOpts::default().with_mmap(true);
        reopen_opts.external_blobs = true;
        let mut reopened =
            MARF::<StacksBlockId>::from_path(clarity_path.to_str().unwrap(), reopen_opts).unwrap();
        assert_eq!(reopened.get_root_hash_at(&block).unwrap(), expected_root);
        drop(reopened);
        let db = Connection::open(&clarity_path).unwrap();
        assert!(
            db.query_row(
                "SELECT EXISTS(SELECT 1 FROM clarity_stable_ptrhash_base)",
                [],
                |row| row.get::<_, bool>(0)
            )
            .unwrap()
        );
        drop(db);
        prepare_for_validation(temporary.path()).unwrap();
    }
}
