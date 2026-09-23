// Copyright (C) 2013-2020 Blockstack PBC, a public benefit corporation
// Copyright (C) 2020-2026 Stacks Open Internet Foundation
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.

//! Persistent and read-only MARF-backed Clarity stores.

use std::ops::DerefMut;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use clarity::util::hash::Sha512Trunc256Sum;
use clarity::vm::database::sqlite::{
    sqlite_get_contract_hash, sqlite_get_metadata, sqlite_get_metadata_manual,
    sqlite_insert_metadata,
};
use clarity::vm::database::{
    ClarityBackingStore, DataStoreEntry, DataStoreValue, SpecialCaseHandler, SqliteConnection,
    StoredValue, StoredValueResult, TypedValueResult,
};
use clarity::vm::errors::{IncomparableError, RuntimeError, VmExecutionError, VmInternalError};
use clarity::vm::types::{QualifiedContractIdentifier, TypeSignature};
use rusqlite::{params, Connection};
use stacks_common::codec::StacksMessageCodec;
use stacks_common::types::chainstate::{BlockHeaderHash, StacksBlockId, TrieHash};
use stacks_common::types::StacksEpochId;

use super::value_extents::{InlineValueRecord, MappedValueRecord, ValueExtentStore};
use crate::chainstate::stacks::index::inline_value::{InlineValue, INLINE_BYTES};
use crate::chainstate::stacks::index::marf::{
    test_override_marf_compression, MARFOpenOpts, MarfConnection, MarfCore, MarfTransaction, MARF,
};
use crate::chainstate::stacks::index::record::NodeRecordFormat;
use crate::chainstate::stacks::index::storage::{TrieFileStorage, TrieHashCalculationMode};
use crate::chainstate::stacks::index::{ClarityMarfTrieId, Error, MARFValue, TrieLeaf};
use crate::clarity_vm::clarity::{
    ClarityMarfStore, ClarityMarfStoreTransaction, WritableMarfStore,
};
use crate::clarity_vm::database::binary_value_store::{self, ValueStorageFormat};
use crate::clarity_vm::database::ephemeral::EphemeralMarfStore;
use crate::clarity_vm::special::handle_contract_call_special_cases_ref;
use crate::core::{FIRST_BURNCHAIN_CONSENSUS_HASH, FIRST_STACKS_BLOCK_HASH};
use crate::util_lib::db::{Error as DatabaseError, IndexDBConn};

/// The MarfedKV struct is used to wrap a MARF data structure and side-storage
///   for use as a K/V store for ClarityDB or the AnalysisDB.
/// The Clarity VM and type checker do not "know" to begin/commit the block they are currently processing:
///   each instantiation of the VM simply executes one transaction. So the block handling
///   loop will need to invoke these two methods (begin + commit) outside of the context of the VM.
///   NOTE: Clarity will panic if you try to execute it from a non-initialized MarfedKV context.
///   (See: vm::tests::with_marfed_environment())
pub struct MarfedKV {
    chain_tip: StacksBlockId,
    marf: MARF<StacksBlockId>,
    /// RAM-backed MARF that will be mutably referenced in an EphemeralMarfStore instance.
    /// Due to limits in Rust's type system, it is necessary for this to be instantiated in
    /// MarfedKV, since it must outlive EphemeralMarfStore, and MarfedKV is the "parent" of
    /// all data referenced by ClarityMarfStore implementations (including the read-only and
    /// persistent MARF stores).
    ephemeral_marf: Option<MARF<StacksBlockId>>,
    value_storage_format: ValueStorageFormat,
    /// Extent mappings shared by the parent and its read/write contexts.
    value_extents: Option<Arc<Mutex<ValueExtentStore>>>,
}

impl MarfedKV {
    fn setup_db(
        path_str: &str,
        unconfirmed: bool,
        marf_opts: Option<MARFOpenOpts>,
    ) -> Result<(MARF<StacksBlockId>, ValueStorageFormat), VmExecutionError> {
        let mut path = PathBuf::from(path_str);

        std::fs::create_dir_all(&path).map_err(|_| VmInternalError::FailedToCreateDataDirectory)?;

        path.push("marf.sqlite");
        let marf_path = path
            .to_str()
            .ok_or_else(|| VmInternalError::BadFileName)?
            .to_string();

        let mut marf_opts = marf_opts.unwrap_or(MARFOpenOpts::default());
        marf_opts.external_blobs = true;

        test_override_marf_compression(&mut marf_opts);

        let mut marf: MARF<StacksBlockId> = if unconfirmed {
            MARF::from_path_unconfirmed(&marf_path, marf_opts)
                .map_err(|err| VmInternalError::MarfFailure(err.to_string()))?
        } else {
            MARF::from_path(&marf_path, marf_opts)
                .map_err(|err| VmInternalError::MarfFailure(err.to_string()))?
        };

        if SqliteConnection::check_schema(marf.sqlite_conn()).is_ok() {
            let value_storage_format = binary_value_store::detect(marf.sqlite_conn())?;
            return Ok((marf, value_storage_format));
        }

        let tx = marf
            .storage_tx()
            .map_err(|err| VmInternalError::DBError(err.to_string()))?;

        SqliteConnection::initialize_conn(&tx)?;
        let value_storage_format = ValueStorageFormat::BinaryV1;
        binary_value_store::initialize_empty(&tx)?;
        tx.commit()
            .map_err(|err| VmInternalError::SqliteError(IncomparableError { err }))?;

        Ok((marf, value_storage_format))
    }

    pub fn open(
        path_str: &str,
        miner_tip: Option<&StacksBlockId>,
        marf_opts: Option<MARFOpenOpts>,
    ) -> Result<MarfedKV, VmExecutionError> {
        let (mut marf, value_storage_format) = MarfedKV::setup_db(path_str, false, marf_opts)?;
        let chain_tip = match miner_tip {
            Some(miner_tip) => miner_tip.clone(),
            None => StacksBlockId::sentinel(),
        };

        let value_extents = open_value_extents(&mut marf)?;
        Ok(MarfedKV {
            marf,
            chain_tip,
            ephemeral_marf: None,
            value_storage_format,
            value_extents,
        })
    }

    pub fn open_unconfirmed(
        path_str: &str,
        miner_tip: Option<&StacksBlockId>,
        marf_opts: Option<MARFOpenOpts>,
    ) -> Result<MarfedKV, VmExecutionError> {
        let (mut marf, value_storage_format) = MarfedKV::setup_db(path_str, true, marf_opts)?;
        let chain_tip = match miner_tip {
            Some(miner_tip) => miner_tip.clone(),
            None => StacksBlockId::sentinel(),
        };

        let value_extents = open_value_extents(&mut marf)?;
        Ok(MarfedKV {
            marf,
            chain_tip,
            ephemeral_marf: None,
            value_storage_format,
            value_extents,
        })
    }

    // used by benchmarks
    pub fn temporary() -> MarfedKV {
        use stacks_common::util::hash::to_hex;

        let mut path = PathBuf::from_str("/tmp/stacks-node-tests/unit-tests-marf").unwrap();
        let random_bytes = rand::random::<[u8; 32]>();
        path.push(to_hex(&random_bytes));

        debug!(
            "Temporary MARF path at {}",
            &path
                .to_str()
                .expect("FATAL: non-UTF-8 character in filename")
        );

        let (mut marf, value_storage_format) = MarfedKV::setup_db(
            path.to_str()
                .expect("Inexplicably non-UTF-8 character in filename"),
            false,
            None,
        )
        .unwrap();

        let chain_tip = StacksBlockId::sentinel();

        let value_extents = open_value_extents(&mut marf).expect("temporary extent store");
        MarfedKV {
            marf,
            chain_tip,
            ephemeral_marf: None,
            value_storage_format,
            value_extents,
        }
    }

    /// Enable direct value extents on an empty Binary V1 store before executing any blocks.
    pub fn enable_value_extents(&mut self) -> Result<(), VmExecutionError> {
        if self.value_extents.is_some() {
            return Ok(());
        }
        if !self.value_storage_format.is_binary() {
            return Err(extent_error(
                "extent activation requires Binary V1 metadata",
            ));
        }
        let count: i64 = self
            .marf
            .sqlite_conn()
            .query_row("SELECT (SELECT COUNT(*) FROM data_table) + (SELECT COUNT(*) FROM marf_data) + (SELECT COUNT(*) FROM mined_blocks)", [], |row| row.get(0))
            .map_err(|error| extent_error(&error.to_string()))?;
        if count != 0 {
            return Err(extent_error(
                "existing values require offline extent migration",
            ));
        }
        let store = ValueExtentStore::open(
            &PathBuf::from(format!("{}.values", self.marf.get_db_path())),
            true,
        )?;
        let tx = self
            .marf
            .storage_tx()
            .map_err(|error| extent_error(&error.to_string()))?;
        tx.execute_batch("CREATE TABLE clarity_extent_format (singleton INTEGER PRIMARY KEY CHECK(singleton = 1), version INTEGER NOT NULL CHECK(version = 1), store_id BLOB NOT NULL CHECK(length(store_id) = 16))")
            .map_err(|error| extent_error(&error.to_string()))?;
        tx.execute(
            "INSERT INTO clarity_extent_format VALUES (1, 1, ?1)",
            params![store.store_id().as_slice()],
        )
        .map_err(|error| extent_error(&error.to_string()))?;
        ValueExtentStore::initialize_index(&tx)?;
        NodeRecordFormat::TypeFirstV4
            .publish(&tx)
            .map_err(|error| extent_error(&error.to_string()))?;
        tx.commit()
            .map_err(|error| extent_error(&error.to_string()))?;
        self.marf.set_record_format(NodeRecordFormat::TypeFirstV4);
        let store = Arc::new(Mutex::new(store));
        self.marf.set_value_extent_resolver(store.clone());
        self.value_extents = Some(store);
        Ok(())
    }

    /// Observe externally committed value-file growth before establishing a new block view.
    fn refresh_value_extents(&mut self) -> Result<(), VmExecutionError> {
        if let Some(store) = &self.value_extents {
            store
                .lock()
                .map_err(|_| extent_error("extent lock poisoned"))?
                .refresh_mapping()?;
        }
        Ok(())
    }

    pub fn begin_read_only<'a>(
        &'a mut self,
        at_block: Option<&StacksBlockId>,
    ) -> ReadOnlyMarfStore<'a> {
        self.refresh_value_extents()
            .expect("failed to refresh value mapping");
        let chain_tip = if let Some(at_block) = at_block {
            self.marf.open_block(at_block).unwrap_or_else(|e| {
                error!(
                    "Failed to open read only connection at {}: {:?}",
                    at_block, &e
                );
                panic!()
            });
            at_block.clone()
        } else {
            self.chain_tip.clone()
        };
        ReadOnlyMarfStore {
            chain_tip,
            marf: &mut self.marf,
            value_storage_format: self.value_storage_format,
            value_extents: self.value_extents.clone(),
        }
    }

    pub fn begin_read_only_checked<'a>(
        &'a mut self,
        at_block: Option<&StacksBlockId>,
    ) -> Result<ReadOnlyMarfStore<'a>, VmExecutionError> {
        self.refresh_value_extents()?;
        let chain_tip = if let Some(at_block) = at_block {
            self.marf.open_block(at_block).map_err(|e| {
                debug!(
                    "Failed to open read only connection at {}: {:?}",
                    at_block, &e
                );
                VmInternalError::MarfFailure(Error::NotFoundError.to_string())
            })?;
            at_block.clone()
        } else {
            self.chain_tip.clone()
        };
        Ok(ReadOnlyMarfStore {
            chain_tip,
            marf: &mut self.marf,
            value_storage_format: self.value_storage_format,
            value_extents: self.value_extents.clone(),
        })
    }

    /// begin, commit, rollback a save point identified by key
    ///    this is used to clean up any data from aborted blocks
    ///     (NOT aborted transactions that is handled by the clarity vm directly).
    /// The block header hash is used for identifying savepoints.
    ///     this _cannot_ be used to rollback to arbitrary prior block hash, because that
    ///     blockhash would already have committed and no longer exist in the save point stack.
    /// this is a "lower-level" rollback than the roll backs performed in
    ///   ClarityDatabase or AnalysisDatabase -- this is done at the backing store level.
    pub fn begin<'a>(
        &'a mut self,
        current: &StacksBlockId,
        next: &StacksBlockId,
    ) -> PersistentWritableMarfStore<'a> {
        if let Some(store) = &self.value_extents {
            store
                .lock()
                .expect("extent lock poisoned")
                .begin_block()
                .expect("failed to refresh value mapping");
        }
        let mut tx = self.marf.begin_tx().unwrap_or_else(|e| {
            panic!(
                "ERROR: Failed to begin new MARF block {} - {}): {:?}",
                current, next, &e
            )
        });
        tx.begin(current, next).unwrap_or_else(|e| {
            panic!(
                "ERROR: Failed to begin new MARF block {} - {}: {:?})",
                current, next, &e
            )
        });

        let chain_tip = tx
            .get_open_chain_tip()
            .expect("ERROR: Failed to get open MARF")
            .clone();

        PersistentWritableMarfStore {
            chain_tip,
            marf: tx,
            value_storage_format: self.value_storage_format,
            value_extents: self.value_extents.clone(),
        }
    }

    pub fn begin_unconfirmed<'a>(
        &'a mut self,
        current: &StacksBlockId,
    ) -> PersistentWritableMarfStore<'a> {
        if let Some(store) = &self.value_extents {
            store
                .lock()
                .expect("extent lock poisoned")
                .begin_block()
                .expect("failed to refresh value mapping");
        }
        let mut tx = self.marf.begin_tx().unwrap_or_else(|_| {
            panic!(
                "ERROR: Failed to begin new unconfirmed MARF block for {})",
                current
            )
        });
        tx.begin_unconfirmed(current).unwrap_or_else(|_| {
            panic!(
                "ERROR: Failed to begin new unconfirmed MARF block for {})",
                current
            )
        });

        let chain_tip = tx
            .get_open_chain_tip()
            .expect("ERROR: Failed to get open MARF")
            .clone();

        PersistentWritableMarfStore {
            chain_tip,
            marf: tx,
            value_storage_format: self.value_storage_format,
            value_extents: self.value_extents.clone(),
        }
    }

    /// Begin an ephemeral MARF block.
    /// The data will never hit disk.
    pub fn begin_ephemeral<'a>(
        &'a mut self,
        base_tip: &StacksBlockId,
        ephemeral_next: &StacksBlockId,
    ) -> Result<EphemeralMarfStore<'a>, VmExecutionError> {
        self.refresh_value_extents()?;
        // sanity check -- `base_tip` must be mapped
        self.marf.open_block(&base_tip).map_err(|e| {
            debug!(
                "Failed to open read only connection at {}: {:?}",
                &base_tip, &e
            );
            VmInternalError::MarfFailure(Error::NotFoundError.to_string())
        })?;

        // set up ephemeral MARF
        let ephemeral_marf_storage = TrieFileStorage::open(
            ":memory:",
            MARFOpenOpts::new(TrieHashCalculationMode::Deferred, false),
        )
        .map_err(|e| {
            VmInternalError::Expect(format!("Failed to instantiate ephemeral MARF: {:?}", &e))
        })?;

        let mut ephemeral_marf = MARF::from_storage(ephemeral_marf_storage);
        let tx = ephemeral_marf
            .storage_tx()
            .map_err(|err| VmInternalError::DBError(err.to_string()))?;

        SqliteConnection::initialize_conn(&tx)?;
        if self.value_storage_format.is_binary() {
            binary_value_store::initialize_empty(&tx)?;
        }
        tx.commit()
            .map_err(|err| VmInternalError::SqliteError(IncomparableError { err }))?;

        self.ephemeral_marf = Some(ephemeral_marf);

        let read_only_marf = ReadOnlyMarfStore {
            chain_tip: base_tip.clone(),
            marf: &mut self.marf,
            value_storage_format: self.value_storage_format,
            value_extents: self.value_extents.clone(),
        };

        let Some(ephemeral_marf) = self.ephemeral_marf.as_mut() else {
            // unreachable since self.ephemeral_marf is already assigned
            unreachable!();
        };

        // attach the disk-backed MARF to the ephemeral MARF
        EphemeralMarfStore::attach_read_only_marf(&ephemeral_marf, &read_only_marf).map_err(
            |e| {
                VmInternalError::Expect(format!(
                    "Failed to attach read-only MARF to ephemeral MARF: {:?}",
                    &e
                ))
            },
        )?;

        let mut tx = ephemeral_marf.begin_tx().map_err(|e| {
            VmInternalError::Expect(format!("Failed to open ephemeral MARF tx: {:?}", &e))
        })?;

        tx.begin(&StacksBlockId::sentinel(), ephemeral_next)
            .map_err(|e| {
                VmInternalError::Expect(format!(
                    "Failed to begin first ephemeral MARF block: {:?}",
                    &e
                ))
            })?;

        let ephemeral_marf_store = EphemeralMarfStore::new(read_only_marf, tx).map_err(|e| {
            VmInternalError::Expect(format!(
                "Failed to instantiate ephemeral MARF store: {:?}",
                &e
            ))
        })?;

        Ok(ephemeral_marf_store)
    }

    pub fn get_chain_tip(&self) -> &StacksBlockId {
        &self.chain_tip
    }

    pub fn get_marf(&mut self) -> &mut MARF<StacksBlockId> {
        &mut self.marf
    }

    #[cfg(test)]
    pub fn sql_conn(&self) -> &Connection {
        self.marf.sqlite_conn()
    }

    pub fn index_conn<C>(&self, context: C) -> IndexDBConn<'_, C, StacksBlockId> {
        IndexDBConn::new(&self.marf, context)
    }
}

/// A wrapper around a MARF transaction which allows read/write access to the MARF's keys off of a
/// given chain tip.
pub struct PersistentWritableMarfStore<'a> {
    /// The chain tip from which reads and writes will be indexed.
    chain_tip: StacksBlockId,
    /// The transaction to the MARF instance
    marf: MarfTransaction<'a, StacksBlockId>,
    /// Immutable physical format selected when the side store opened.
    value_storage_format: ValueStorageFormat,
    /// Extent mappings shared by the parent and its read/write contexts.
    value_extents: Option<Arc<Mutex<ValueExtentStore>>>,
}

/// A wrapper around a MARF handle which allows only read access to the MARF's keys off of a given
/// chain tip.
pub struct ReadOnlyMarfStore<'a> {
    /// The chain tip from which reads will be indexed.
    chain_tip: StacksBlockId,
    /// Handle to the MARF being read
    marf: &'a mut MARF<StacksBlockId>,
    /// Immutable physical format selected when the side store opened.
    value_storage_format: ValueStorageFormat,
    /// Extent mappings shared by the parent and its read/write contexts.
    value_extents: Option<Arc<Mutex<ValueExtentStore>>>,
}

impl ClarityMarfStore for ReadOnlyMarfStore<'_> {}
impl ClarityMarfStore for PersistentWritableMarfStore<'_> {}

/// Load a required typed value from Binary V1 after MARF traversal resolves its hash.
fn get_typed_side_value(
    conn: &Connection,
    marf_value: &MARFValue,
    expected: &TypeSignature,
    epoch: &StacksEpochId,
) -> Result<TypedValueResult, VmExecutionError> {
    binary_value_store::get_typed(conn, marf_value, expected, epoch)?.ok_or_else(|| {
        VmInternalError::Expect(format!(
            "ERROR: MARF contained value_hash not found in Binary V1 side storage: {}",
            marf_value.to_hex()
        ))
        .into()
    })
}

/// Load a required shared typed value after MARF traversal resolves its hash.
fn get_stored_side_value(
    conn: &Connection,
    marf_value: &MARFValue,
    expected: &TypeSignature,
    epoch: &StacksEpochId,
) -> Result<StoredValueResult, VmExecutionError> {
    binary_value_store::get_stored(conn, marf_value, expected, epoch)?.ok_or_else(|| {
        VmInternalError::Expect(format!(
            "ERROR: MARF contained value_hash not found in Binary V1 side storage: {}",
            marf_value.to_hex()
        ))
        .into()
    })
}

/// Load an optional canonical value through the database's immutable physical format.
fn get_side_value(
    conn: &Connection,
    mode: ValueStorageFormat,
    marf_value: &MARFValue,
) -> Result<Option<String>, VmExecutionError> {
    if mode.is_binary() {
        binary_value_store::get_generic(conn, marf_value)
    } else {
        SqliteConnection::get(conn, &marf_value.to_hex())
    }
}

/// Load a canonical value and report a missing side-store row as MARF corruption.
fn get_required_side_value(
    mode: ValueStorageFormat,
    conn: &Connection,
    marf_value: &MARFValue,
) -> Result<String, VmExecutionError> {
    get_side_value(conn, mode, marf_value)?.ok_or_else(|| {
        VmInternalError::Expect(format!(
            "ERROR: MARF contained value_hash not found in side storage: {}",
            marf_value.to_hex()
        ))
        .into()
    })
}

/// Decode a legacy canonical-hex value under the caller's declared read schema.
fn decode_legacy_typed(
    canonical: String,
    expected: &TypeSignature,
    epoch: &StacksEpochId,
) -> Result<TypedValueResult, VmExecutionError> {
    let serialized_byte_len = canonical.len() as u64 / 2;
    let value =
        clarity::vm::types::Value::try_deserialize_hex_at_epoch(&canonical, expected, epoch)
            .map_err(|error| VmInternalError::DBError(error.to_string()))?;
    Ok(TypedValueResult {
        value,
        serialized_byte_len,
    })
}

impl ClarityMarfStoreTransaction for PersistentWritableMarfStore<'_> {
    /// Commit metadata for a given `target` trie.  In this MARF store, this just renames all
    /// metadata rows with `self.chain_tip` as their block identifier to have `target` instead.
    ///
    /// Returns Ok(()) on success
    /// Returns Err(VmInternalError(..)) on sqlite failure
    fn commit_metadata_for_trie(&mut self, target: &StacksBlockId) -> Result<(), VmExecutionError> {
        if self.value_storage_format.is_binary() {
            binary_value_store::commit_metadata_to(
                self.marf.sqlite_tx(),
                self.chain_tip.as_bytes(),
                target.as_bytes(),
            )
        } else {
            SqliteConnection::commit_metadata_to(self.marf.sqlite_tx(), &self.chain_tip, target)
        }
    }

    /// Drop metadata for the given `target` trie. This just drops the metadata rows with `target`
    /// as their block identifier.
    ///
    /// Returns Ok(()) on success
    /// Returns Err(VmInternalError(..)) on sqlite failure
    fn drop_metadata_for_trie(&mut self, target: &StacksBlockId) -> Result<(), VmExecutionError> {
        if self.value_storage_format.is_binary() {
            binary_value_store::drop_metadata(self.marf.sqlite_tx(), target.as_bytes())
        } else {
            SqliteConnection::drop_metadata(self.marf.sqlite_tx(), target)
        }
    }

    /// Seal the trie -- compute the root hash.
    /// NOTE: This is a one-time operation for this implementation -- a subsequent call will panic.
    fn seal_trie(&mut self) -> TrieHash {
        self.marf
            .seal()
            .expect("FATAL: failed to .seal() MARF transaction")
    }

    /// Drop the trie being built. This just drops the data from RAM and aborts the underlying
    /// sqlite transaction.  This instance is consumed.
    fn drop_current_trie(self) {
        if let Some(store) = &self.value_extents {
            store.lock().expect("extent lock poisoned").discard_block();
        }
        self.marf.drop_current();
    }

    /// Drop unconfirmed state being built. This will not only drop unconfirmed state in RAM, but
    /// also any unconfirmed trie data from the sqlite DB as well as its associated metadata.
    ///
    /// Returns Ok(()) on success
    /// Returns Err(VmInternalError(..)) on sqlite failure
    fn drop_unconfirmed(mut self) -> Result<(), VmExecutionError> {
        let chain_tip = self.chain_tip.clone();
        debug!("Drop unconfirmed MARF trie {}", &chain_tip);
        self.drop_metadata_for_trie(&chain_tip)?;
        self.marf.drop_unconfirmed();
        if let Some(store) = &self.value_extents {
            store.lock().expect("extent lock poisoned").discard_block();
        }
        Ok(())
    }

    /// Commit the outstanding trie and metadata to the set of processed-block tries, and call it
    /// `target` in the DB.  Future tries can be built atop it.  This commits the transaction and
    /// drops this MARF store.
    ///
    /// Returns Ok(()) on success
    /// Returns Err(VmInternalError(..)) on sqlite failure
    fn commit_to_processed_block(mut self, target: &StacksBlockId) -> Result<(), VmExecutionError> {
        debug!("commit_to({})", target);
        self.commit_metadata_for_trie(target)?;
        if let Some(store) = &self.value_extents {
            store
                .lock()
                .map_err(|_| extent_error("extent lock poisoned"))?
                .publish_block()?;
        }
        let _ = self.marf.commit_to(target).map_err(|e| {
            error!("Failed to commit to MARF block {target}: {e:?}");
            VmInternalError::Expect("Failed to commit to MARF block".into())
        })?;
        if let Some(store) = &self.value_extents {
            store
                .lock()
                .expect("extent lock poisoned")
                .commit_dedup_cache();
        }
        Ok(())
    }

    /// Commit the outstanding trie to the `mined_blocks` table in the underlying MARF.
    /// The metadata will be dropped, since this won't be added to the chainstate.  This commits
    /// the transaction and drops this MARF store.
    ///
    /// Returns Ok(()) on success
    /// Returns Err(VmInternalError(..)) on sqlite failure
    fn commit_to_mined_block(mut self, target: &StacksBlockId) -> Result<(), VmExecutionError> {
        debug!("commit_mined_block: ({}->{})", &self.chain_tip, target);
        // rollback the side_store
        //    the side_store shouldn't commit data for blocks that won't be
        //    included in the processed chainstate (like a block constructed during mining)
        //    _if_ for some reason, we do want to be able to access that mined chain state in the future,
        //    we should probably commit the data to a different table which does not have uniqueness constraints.
        let chain_tip = self.chain_tip.clone();
        self.drop_metadata_for_trie(&chain_tip)?;
        if let Some(store) = &self.value_extents {
            store
                .lock()
                .map_err(|_| extent_error("extent lock poisoned"))?
                .publish_block()?;
        }
        let _ = self.marf.commit_mined(target).map_err(|e| {
            error!("Failed to commit to mined MARF block {target}: {e:?}",);
            VmInternalError::Expect("Failed to commit to MARF block".into())
        })?;
        if let Some(store) = &self.value_extents {
            store
                .lock()
                .expect("extent lock poisoned")
                .commit_dedup_cache();
        }
        Ok(())
    }

    /// Commit the outstanding trie to unconfirmed state, so subsequent read I/O can be performed
    /// on it (such as servicing RPC requests).  This commits this transaction and drops this MARF
    /// store
    fn commit_unconfirmed(self) {
        debug!("commit_unconfirmed()");
        if let Some(store) = &self.value_extents {
            store
                .lock()
                .expect("extent lock poisoned")
                .publish_block()
                .expect("ERROR: Failed to publish value extent mapping");
        }
        // NOTE: Can omit commit_metadata_to, since the block header hash won't change
        self.marf
            .commit()
            .expect("ERROR: Failed to commit MARF block");
        if let Some(store) = &self.value_extents {
            store
                .lock()
                .expect("extent lock poisoned")
                .commit_dedup_cache();
        }
    }

    #[cfg(test)]
    fn test_commit(self) {
        self.do_test_commit()
    }
}

impl ReadOnlyMarfStore<'_> {
    /// Whether disk-backed values use physical extents instead of SQLite rows.
    pub fn uses_value_extents(&self) -> bool {
        self.value_extents.is_some()
    }

    /// Return the physical Clarity value-storage mode used by this store.
    pub fn value_storage_format(&self) -> ValueStorageFormat {
        self.value_storage_format
    }

    /// Determine if there is a trie in the underlying MARF with the given ID `bhh`.
    ///
    /// Return Ok(true) if so
    /// Return Ok(false) if not
    /// Return Err(..) if we encounter a sqlite error
    pub fn trie_exists_for_block(&mut self, bhh: &StacksBlockId) -> Result<bool, DatabaseError> {
        self.marf
            .with_conn(|conn| conn.has_block(bhh).map_err(DatabaseError::IndexError))
    }

    /// Get the DB path on disk.
    /// If the DB is in RAM, this will be ":memory:"
    pub fn get_db_path(&self) -> &str {
        self.marf.get_db_path()
    }

    /// Get a reference to the chain tip
    pub fn get_chain_tip(&self) -> &StacksBlockId {
        &self.chain_tip
    }

    /// Helper wrapper around MARF::check_ancestor_block_hash(),
    pub fn check_ancestor_block_hash(&mut self, bhh: &StacksBlockId) -> Result<(), Error> {
        self.marf.check_ancestor_block_hash(bhh)
    }
}

impl ClarityBackingStore for ReadOnlyMarfStore<'_> {
    #[stacks_profiler::profile(name = "Clarity backing typed read")]
    fn get_typed_value(
        &mut self,
        key: &str,
        expected: &TypeSignature,
        epoch: &StacksEpochId,
    ) -> Result<Option<TypedValueResult>, VmExecutionError> {
        if let Some(store) = &self.value_extents {
            let leaf = self
                .marf
                .get_leaf_by_key(&self.chain_tip, key)
                .map_err(|error| extent_error(&error.to_string()))?;
            return leaf
                .map(|leaf| {
                    let record = read_value_extent(store, &leaf)?;
                    let stored = record.stored(expected, epoch)?;
                    let value = match stored.value {
                        StoredValue::Owned(value) => value,
                        StoredValue::Packed(value) => value
                            .to_owned_value()
                            .map_err(|error| extent_error(&error.to_string()))?,
                    };
                    Ok(TypedValueResult {
                        value,
                        serialized_byte_len: stored.serialized_byte_len,
                    })
                })
                .transpose();
        }
        if !self.value_storage_format.is_binary() {
            return self
                .get_data(key)?
                .map(|canonical| decode_legacy_typed(canonical, expected, epoch))
                .transpose();
        }
        self.marf
            .get(&self.chain_tip, key)
            .or_else(|error| match error {
                Error::NotFoundError => Ok(None),
                _ => Err(error),
            })
            .map_err(|_| VmInternalError::Expect("ERROR: Unexpected MARF Failure on GET".into()))?
            .map(|marf_value| {
                get_typed_side_value(self.get_side_store(), &marf_value, expected, epoch)
            })
            .transpose()
    }

    #[stacks_profiler::profile(name = "Clarity backing stored read")]
    fn get_stored_value(
        &mut self,
        key: &str,
        expected: &TypeSignature,
        epoch: &StacksEpochId,
    ) -> Result<Option<StoredValueResult>, VmExecutionError> {
        if let Some(store) = &self.value_extents {
            let leaf = self
                .marf
                .get_leaf_by_key(&self.chain_tip, key)
                .map_err(|error| extent_error(&error.to_string()))?;
            return leaf
                .map(|leaf| {
                    let record = read_value_extent(store, &leaf)?;
                    record.stored(expected, epoch)
                })
                .transpose();
        }
        if !self.value_storage_format.is_binary() {
            return self.get_typed_value(key, expected, epoch).map(|result| {
                result.map(|result| StoredValueResult {
                    value: StoredValue::Owned(result.value),
                    serialized_byte_len: result.serialized_byte_len,
                })
            });
        }
        self.marf
            .get(&self.chain_tip, key)
            .or_else(|error| match error {
                Error::NotFoundError => Ok(None),
                _ => Err(error),
            })
            .map_err(|_| VmInternalError::Expect("ERROR: Unexpected MARF Failure on GET".into()))?
            .map(|marf_value| {
                get_stored_side_value(self.get_side_store(), &marf_value, expected, epoch)
            })
            .transpose()
    }

    fn get_side_store(&mut self) -> &Connection {
        self.marf.sqlite_conn()
    }

    fn get_cc_special_cases_handler(&self) -> Option<SpecialCaseHandler> {
        Some(&handle_contract_call_special_cases_ref)
    }

    /// Sets the chain tip at which queries will happen.  Used for `(at-block ..)`
    fn set_block_hash(&mut self, bhh: StacksBlockId) -> Result<StacksBlockId, VmExecutionError> {
        self.marf
            .check_ancestor_block_hash(&bhh)
            .map_err(|e| match e {
                Error::NotFoundError => {
                    test_debug!("No such block {:?} (NotFoundError)", &bhh);
                    RuntimeError::UnknownBlockHeaderHash(BlockHeaderHash(bhh.0))
                }
                Error::NonMatchingForks(_bh1, _bh2) => {
                    test_debug!(
                        "No such block {:?} (NonMatchingForks({}, {}))",
                        &bhh,
                        BlockHeaderHash(_bh1),
                        BlockHeaderHash(_bh2)
                    );
                    RuntimeError::UnknownBlockHeaderHash(BlockHeaderHash(bhh.0))
                }
                _ => panic!("ERROR: Unexpected MARF failure: {}", e),
            })?;

        let result = Ok(self.chain_tip.clone());
        self.chain_tip = bhh;

        result
    }

    fn get_current_block_height(&mut self) -> u32 {
        match self
            .marf
            .get_block_height_of(&self.chain_tip, &self.chain_tip)
        {
            Ok(Some(x)) => x,
            Ok(None) => {
                let first_tip =
                    StacksBlockId::new(&FIRST_BURNCHAIN_CONSENSUS_HASH, &FIRST_STACKS_BLOCK_HASH);
                if self.chain_tip == first_tip || self.chain_tip == StacksBlockId([0u8; 32]) {
                    // the current block height should always work, except if it's the first block
                    // height (in which case, the current chain tip should match the first-ever
                    // index block hash).
                    return 0;
                }

                // should never happen
                let msg = format!(
                    "Failed to obtain current block height of {} (got None)",
                    &self.chain_tip
                );
                error!("{}", &msg);
                panic!("{}", &msg);
            }
            Err(e) => {
                let msg = format!(
                    "Unexpected MARF failure: Failed to get current block height of {}: {:?}",
                    &self.chain_tip, &e
                );
                error!("{}", &msg);
                panic!("{}", &msg);
            }
        }
    }

    fn get_block_at_height(&mut self, block_height: u32) -> Option<StacksBlockId> {
        self.marf
            .get_bhh_at_height(&self.chain_tip, block_height)
            .unwrap_or_else(|_| {
                panic!(
                    "Unexpected MARF failure: failed to get block at height {} off of {}.",
                    block_height, &self.chain_tip
                )
            })
            .map(|x| StacksBlockId(x.to_bytes()))
    }

    fn get_open_chain_tip(&mut self) -> StacksBlockId {
        StacksBlockId(
            self.marf
                .get_open_chain_tip()
                .expect("Attempted to get the open chain tip from an unopened context.")
                .clone()
                .to_bytes(),
        )
    }

    fn get_open_chain_tip_height(&mut self) -> u32 {
        self.marf
            .get_open_chain_tip_height()
            .expect("Attempted to get the open chain tip from an unopened context.")
    }

    fn get_data_with_proof(
        &mut self,
        key: &str,
    ) -> Result<Option<(String, Vec<u8>)>, VmExecutionError> {
        let proof_path = TrieHash::from_key(key);
        self.marf
            .get_with_proof(&self.chain_tip, key)
            .or_else(|e| match e {
                Error::NotFoundError => Ok(None),
                _ => Err(e),
            })
            .map_err(|_| VmInternalError::Expect("ERROR: Unexpected MARF Failure on GET".into()))?
            .map(|(marf_value, proof)| {
                let data = if self.value_extents.is_some() {
                    self.get_data_from_path(&proof_path)?
                        .ok_or_else(|| extent_error("proof leaf value missing"))?
                } else {
                    get_required_side_value(
                        self.value_storage_format,
                        self.get_side_store(),
                        &marf_value,
                    )?
                };
                Ok((data, proof.serialize_to_vec()))
            })
            .transpose()
    }

    fn get_data_with_proof_from_path(
        &mut self,
        hash: &TrieHash,
    ) -> Result<Option<(String, Vec<u8>)>, VmExecutionError> {
        let proof_path = hash.clone();
        self.marf
            .get_with_proof_from_hash(&self.chain_tip, hash)
            .or_else(|e| match e {
                Error::NotFoundError => Ok(None),
                _ => Err(e),
            })
            .map_err(|_| VmInternalError::Expect("ERROR: Unexpected MARF Failure on GET".into()))?
            .map(|(marf_value, proof)| {
                let data = if self.value_extents.is_some() {
                    self.get_data_from_path(&proof_path)?
                        .ok_or_else(|| extent_error("proof leaf value missing"))?
                } else {
                    get_required_side_value(
                        self.value_storage_format,
                        self.get_side_store(),
                        &marf_value,
                    )?
                };
                Ok((data, proof.serialize_to_vec()))
            })
            .transpose()
    }

    #[stacks_profiler::profile(name = "Clarity backing read key")]
    fn get_data(&mut self, key: &str) -> Result<Option<String>, VmExecutionError> {
        if let Some(store) = &self.value_extents {
            let leaf = self
                .marf
                .get_leaf_by_key(&self.chain_tip, key)
                .map_err(|error| extent_error(&error.to_string()))?;
            return leaf
                .map(|leaf| {
                    let record = read_value_extent(store, &leaf)?;
                    record.canonical()
                })
                .transpose();
        }
        self.marf
            .get(&self.chain_tip, key)
            .or_else(|e| match e {
                Error::NotFoundError => {
                    test_debug!(
                        "ReadOnly MarfedKV get {:?} off of {:?}: not found",
                        key,
                        &self.chain_tip
                    );
                    Ok(None)
                }
                _ => {
                    test_debug!(
                        "ReadOnly MarfedKV get {:?} off of {:?}: {:?}",
                        key,
                        &self.chain_tip,
                        &e
                    );
                    Err(e)
                }
            })
            .map_err(|_| VmInternalError::Expect("ERROR: Unexpected MARF Failure on GET".into()))?
            .map(|marf_value| {
                get_required_side_value(
                    self.value_storage_format,
                    self.get_side_store(),
                    &marf_value,
                )
            })
            .transpose()
    }

    #[stacks_profiler::profile(name = "Clarity backing read hash")]
    fn get_data_from_path(&mut self, hash: &TrieHash) -> Result<Option<String>, VmExecutionError> {
        if let Some(store) = &self.value_extents {
            let leaf = self
                .marf
                .get_leaf(&self.chain_tip, &hash.clone())
                .map_err(|error| extent_error(&error.to_string()))?;
            return leaf
                .map(|leaf| {
                    let record = read_value_extent(store, &leaf)?;
                    record.canonical()
                })
                .transpose();
        }
        trace!("MarfedKV get_from_hash: {:?} tip={}", hash, &self.chain_tip);
        self.marf
            .get_from_hash(&self.chain_tip, hash)
            .or_else(|e| match e {
                Error::NotFoundError => {
                    trace!(
                        "MarfedKV get {:?} off of {:?}: not found",
                        hash,
                        &self.chain_tip
                    );
                    Ok(None)
                }
                _ => Err(e),
            })
            .map_err(|_| VmInternalError::Expect("ERROR: Unexpected MARF Failure on GET".into()))?
            .map(|marf_value| {
                trace!("MarfedKV get side-key for {:?}: {:?}", hash, marf_value);
                get_required_side_value(
                    self.value_storage_format,
                    self.get_side_store(),
                    &marf_value,
                )
            })
            .transpose()
    }

    fn put_all_data(&mut self, _items: Vec<(String, String)>) -> Result<(), VmExecutionError> {
        error!("Attempted to commit changes to read-only MARF");
        panic!("BUG: attempted commit to read-only MARF");
    }

    fn get_contract_hash(
        &mut self,
        contract: &QualifiedContractIdentifier,
    ) -> Result<(StacksBlockId, Sha512Trunc256Sum), VmExecutionError> {
        sqlite_get_contract_hash(self, contract)
    }

    fn insert_metadata(
        &mut self,
        _contract: &QualifiedContractIdentifier,
        _key: &str,
        _value: &str,
    ) -> Result<(), VmExecutionError> {
        error!("Attempted to commit metadata changes to read-only MARF");
        panic!("BUG: attempted metadata commit to read-only MARF");
    }

    fn get_metadata(
        &mut self,
        contract: &QualifiedContractIdentifier,
        key: &str,
    ) -> Result<Option<String>, VmExecutionError> {
        if self.value_storage_format.is_binary() {
            binary_value_store::get_store_metadata(self, contract, key)
        } else {
            sqlite_get_metadata(self, contract, key)
        }
    }

    fn get_metadata_manual(
        &mut self,
        at_height: u32,
        contract: &QualifiedContractIdentifier,
        key: &str,
    ) -> Result<Option<String>, VmExecutionError> {
        if self.value_storage_format.is_binary() {
            binary_value_store::get_store_metadata_manual(self, at_height, contract, key)
        } else {
            sqlite_get_metadata_manual(self, at_height, contract, key)
        }
    }
}

impl PersistentWritableMarfStore<'_> {
    #[cfg(test)]
    fn do_test_commit(self) {
        let bhh = self.chain_tip.clone();
        self.commit_to_processed_block(&bhh).unwrap();
    }
}

impl ClarityBackingStore for PersistentWritableMarfStore<'_> {
    fn stores_typed_values(&self) -> bool {
        self.value_storage_format.is_binary()
    }

    #[stacks_profiler::profile(name = "Clarity backing typed read")]
    fn get_typed_value(
        &mut self,
        key: &str,
        expected: &TypeSignature,
        epoch: &StacksEpochId,
    ) -> Result<Option<TypedValueResult>, VmExecutionError> {
        if let Some(store) = &self.value_extents {
            let leaf = self
                .marf
                .get_leaf_by_key(&self.chain_tip, key)
                .map_err(|error| extent_error(&error.to_string()))?;
            return leaf
                .map(|leaf| {
                    let record = read_value_extent(store, &leaf)?;
                    let stored = record.stored(expected, epoch)?;
                    let value = match stored.value {
                        StoredValue::Owned(value) => value,
                        StoredValue::Packed(value) => value
                            .to_owned_value()
                            .map_err(|error| extent_error(&error.to_string()))?,
                    };
                    Ok(TypedValueResult {
                        value,
                        serialized_byte_len: stored.serialized_byte_len,
                    })
                })
                .transpose();
        }
        if !self.value_storage_format.is_binary() {
            return self
                .get_data(key)?
                .map(|canonical| decode_legacy_typed(canonical, expected, epoch))
                .transpose();
        }
        self.marf
            .get(&self.chain_tip, key)
            .or_else(|error| match error {
                Error::NotFoundError => Ok(None),
                _ => Err(error),
            })
            .map_err(|_| VmInternalError::Expect("ERROR: Unexpected MARF Failure on GET".into()))?
            .map(|marf_value| {
                get_typed_side_value(self.marf.sqlite_tx(), &marf_value, expected, epoch)
            })
            .transpose()
    }

    #[stacks_profiler::profile(name = "Clarity backing stored read")]
    fn get_stored_value(
        &mut self,
        key: &str,
        expected: &TypeSignature,
        epoch: &StacksEpochId,
    ) -> Result<Option<StoredValueResult>, VmExecutionError> {
        if let Some(store) = &self.value_extents {
            let leaf = self
                .marf
                .get_leaf_by_key(&self.chain_tip, key)
                .map_err(|error| extent_error(&error.to_string()))?;
            return leaf
                .map(|leaf| {
                    let record = read_value_extent(store, &leaf)?;
                    record.stored(expected, epoch)
                })
                .transpose();
        }
        if !self.value_storage_format.is_binary() {
            return self.get_typed_value(key, expected, epoch).map(|result| {
                result.map(|result| StoredValueResult {
                    value: StoredValue::Owned(result.value),
                    serialized_byte_len: result.serialized_byte_len,
                })
            });
        }
        self.marf
            .get(&self.chain_tip, key)
            .or_else(|error| match error {
                Error::NotFoundError => Ok(None),
                _ => Err(error),
            })
            .map_err(|_| VmInternalError::Expect("ERROR: Unexpected MARF Failure on GET".into()))?
            .map(|marf_value| {
                get_stored_side_value(self.marf.sqlite_tx(), &marf_value, expected, epoch)
            })
            .transpose()
    }

    fn set_block_hash(&mut self, bhh: StacksBlockId) -> Result<StacksBlockId, VmExecutionError> {
        self.marf
            .check_ancestor_block_hash(&bhh)
            .map_err(|e| match e {
                Error::NotFoundError => {
                    // This branch will almost never be hit in normal execution because
                    // the MARF always contains at least the genesis block and subsequent
                    // blocks. NotFoundError only occurs if the backing store is completely
                    // empty or uninitialized, which is not possible during normal contract
                    // deployment or runtime execution.
                    test_debug!("No such block {:?} (NotFoundError)", &bhh);
                    RuntimeError::UnknownBlockHeaderHash(BlockHeaderHash(bhh.0))
                }
                Error::NonMatchingForks(_bh1, _bh2) => {
                    test_debug!(
                        "No such block {:?} (NonMatchingForks({}, {}))",
                        &bhh,
                        BlockHeaderHash(_bh1),
                        BlockHeaderHash(_bh2)
                    );
                    RuntimeError::UnknownBlockHeaderHash(BlockHeaderHash(bhh.0))
                }
                _ => panic!("ERROR: Unexpected MARF failure: {}", e),
            })?;

        let result = Ok(self.chain_tip.clone());
        self.chain_tip = bhh;

        result
    }

    fn get_cc_special_cases_handler(&self) -> Option<SpecialCaseHandler> {
        Some(&handle_contract_call_special_cases_ref)
    }

    #[stacks_profiler::profile(name = "Clarity backing read key")]
    fn get_data(&mut self, key: &str) -> Result<Option<String>, VmExecutionError> {
        if let Some(store) = &self.value_extents {
            let leaf = self
                .marf
                .get_leaf_by_key(&self.chain_tip, key)
                .map_err(|error| extent_error(&error.to_string()))?;
            return leaf
                .map(|leaf| {
                    let record = read_value_extent(store, &leaf)?;
                    record.canonical()
                })
                .transpose();
        }
        trace!("MarfedKV get: {:?} tip={}", key, &self.chain_tip);
        self.marf
            .get(&self.chain_tip, key)
            .or_else(|e| match e {
                Error::NotFoundError => {
                    trace!(
                        "MarfedKV get {:?} off of {:?}: not found",
                        key,
                        &self.chain_tip
                    );
                    Ok(None)
                }
                _ => Err(e),
            })
            .map_err(|_| VmInternalError::Expect("ERROR: Unexpected MARF Failure on GET".into()))?
            .map(|marf_value| {
                trace!("MarfedKV get side-key for {:?}: {:?}", key, marf_value);
                get_required_side_value(
                    self.value_storage_format,
                    self.marf.sqlite_tx(),
                    &marf_value,
                )
            })
            .transpose()
    }

    #[stacks_profiler::profile(name = "Clarity backing read hash")]
    fn get_data_from_path(&mut self, hash: &TrieHash) -> Result<Option<String>, VmExecutionError> {
        if let Some(store) = &self.value_extents {
            let leaf = self
                .marf
                .get_leaf(&self.chain_tip, &hash.clone())
                .map_err(|error| extent_error(&error.to_string()))?;
            return leaf
                .map(|leaf| {
                    let record = read_value_extent(store, &leaf)?;
                    record.canonical()
                })
                .transpose();
        }
        trace!("MarfedKV get_from_hash: {:?} tip={}", hash, &self.chain_tip);
        self.marf
            .get_from_hash(&self.chain_tip, hash)
            .or_else(|e| match e {
                Error::NotFoundError => {
                    trace!(
                        "MarfedKV get {:?} off of {:?}: not found",
                        hash,
                        &self.chain_tip
                    );
                    Ok(None)
                }
                _ => Err(e),
            })
            .map_err(|_| VmInternalError::Expect("ERROR: Unexpected MARF Failure on GET".into()))?
            .map(|marf_value| {
                trace!("MarfedKV get side-key for {:?}: {:?}", hash, marf_value);
                get_required_side_value(
                    self.value_storage_format,
                    self.marf.sqlite_tx(),
                    &marf_value,
                )
            })
            .transpose()
    }

    fn get_data_with_proof(
        &mut self,
        key: &str,
    ) -> Result<Option<(String, Vec<u8>)>, VmExecutionError> {
        let proof_path = TrieHash::from_key(key);
        self.marf
            .get_with_proof(&self.chain_tip, key)
            .or_else(|e| match e {
                Error::NotFoundError => Ok(None),
                _ => Err(e),
            })
            .map_err(|_| VmInternalError::Expect("ERROR: Unexpected MARF Failure on GET".into()))?
            .map(|(marf_value, proof)| {
                let data = if self.value_extents.is_some() {
                    self.get_data_from_path(&proof_path)?
                        .ok_or_else(|| extent_error("proof leaf value missing"))?
                } else {
                    get_required_side_value(
                        self.value_storage_format,
                        self.marf.sqlite_tx(),
                        &marf_value,
                    )?
                };
                Ok((data, proof.serialize_to_vec()))
            })
            .transpose()
    }

    fn get_data_with_proof_from_path(
        &mut self,
        hash: &TrieHash,
    ) -> Result<Option<(String, Vec<u8>)>, VmExecutionError> {
        let proof_path = hash.clone();
        self.marf
            .get_with_proof_from_hash(&self.chain_tip, hash)
            .or_else(|e| match e {
                Error::NotFoundError => Ok(None),
                _ => Err(e),
            })
            .map_err(|_| VmInternalError::Expect("ERROR: Unexpected MARF Failure on GET".into()))?
            .map(|(marf_value, proof)| {
                let data = if self.value_extents.is_some() {
                    self.get_data_from_path(&proof_path)?
                        .ok_or_else(|| extent_error("proof leaf value missing"))?
                } else {
                    get_required_side_value(
                        self.value_storage_format,
                        self.marf.sqlite_tx(),
                        &marf_value,
                    )?
                };
                Ok((data, proof.serialize_to_vec()))
            })
            .transpose()
    }

    fn get_side_store(&mut self) -> &Connection {
        self.marf.sqlite_tx()
    }

    fn get_block_at_height(&mut self, height: u32) -> Option<StacksBlockId> {
        self.marf
            .get_block_at_height(height, &self.chain_tip)
            .unwrap_or_else(|_| {
                panic!(
                    "Unexpected MARF failure: failed to get block at height {} off of {}.",
                    height, &self.chain_tip
                )
            })
    }

    fn get_open_chain_tip(&mut self) -> StacksBlockId {
        self.marf
            .get_open_chain_tip()
            .expect("Attempted to get the open chain tip from an unopened context.")
            .clone()
    }

    fn get_open_chain_tip_height(&mut self) -> u32 {
        self.marf
            .get_open_chain_tip_height()
            .expect("Attempted to get the open chain tip from an unopened context.")
    }

    fn get_current_block_height(&mut self) -> u32 {
        match self
            .marf
            .get_block_height_of(&self.chain_tip, &self.chain_tip)
        {
            Ok(Some(x)) => x,
            Ok(None) => {
                let first_tip =
                    StacksBlockId::new(&FIRST_BURNCHAIN_CONSENSUS_HASH, &FIRST_STACKS_BLOCK_HASH);
                if self.chain_tip == first_tip || self.chain_tip == StacksBlockId([0u8; 32]) {
                    // the current block height should always work, except if it's the first block
                    // height (in which case, the current chain tip should match the first-ever
                    // index block hash).
                    return 0;
                }

                // should never happen
                let msg = format!(
                    "Failed to obtain current block height of {} (got None)",
                    &self.chain_tip
                );
                error!("{}", &msg);
                panic!("{}", &msg);
            }
            Err(e) => {
                let msg = format!(
                    "Unexpected MARF failure: Failed to get current block height of {}: {:?}",
                    &self.chain_tip, &e
                );
                error!("{}", &msg);
                panic!("{}", &msg);
            }
        }
    }

    fn put_all_data(&mut self, items: Vec<(String, String)>) -> Result<(), VmExecutionError> {
        if self.value_extents.is_some() {
            return self.put_all_data_entries(
                items
                    .into_iter()
                    .map(|(key, value)| DataStoreEntry {
                        key,
                        value: DataStoreValue::Canonical(value),
                    })
                    .collect(),
            );
        }
        if self.value_storage_format.is_binary() {
            let entries = items
                .into_iter()
                .map(|(key, canonical)| DataStoreEntry {
                    key,
                    value: DataStoreValue::Canonical(canonical),
                })
                .collect();
            let (keys, values) = binary_value_store::put_entries(self.marf.sqlite_tx(), entries)?;
            return self.marf.insert_batch(&keys, values).map_err(|_| {
                VmInternalError::Expect("ERROR: Unexpected MARF Failure".into()).into()
            });
        }

        let mut keys = Vec::with_capacity(items.len());
        let mut values = Vec::with_capacity(items.len());
        for (key, value) in items {
            let marf_value = MARFValue::from_value(&value);
            SqliteConnection::put(self.marf.sqlite_tx(), &marf_value.to_hex(), &value)?;
            keys.push(key);
            values.push(marf_value);
        }
        self.marf
            .insert_batch(&keys, values)
            .map_err(|_| VmInternalError::Expect("ERROR: Unexpected MARF Failure".into()).into())
    }

    fn put_all_data_entries(
        &mut self,
        entries: Vec<DataStoreEntry>,
    ) -> Result<(), VmExecutionError> {
        let _phase = stacks_profiler::diagnostic_span!("Writeback: Apply entries");
        stacks_profiler::diagnostics::count("write_entries", entries.len() as u64);
        if let Some(store) = &self.value_extents {
            let inline_enabled = matches!(self.marf.record_format(), NodeRecordFormat::TypeFirstV3 | NodeRecordFormat::TypeFirstV4);
            let mut keys = Vec::with_capacity(entries.len());
            let mut leaves = Vec::with_capacity(entries.len());
            let mut values = Vec::new();
            let mut encoded = Vec::new();
            let mut extent_slots = Vec::new();
            for entry in entries {
                keys.push(entry.key);
                let prepared = if inline_enabled {
                    binary_value_store::encode_inline_candidate(&entry.value, INLINE_BYTES)?
                } else {
                    None
                };
                if let Some(record) = prepared.as_ref().filter(|record| {
                    InlineValue::fits_inline(
                        record.record().len(),
                        record.shape().map_or(0, <[u8]>::len),
                    )
                }) {
                    let mut leaf =
                        TrieLeaf::from_value(&[], MARFValue::from_value(entry.value.canonical()));
                    leaf.inline = Some(
                        InlineValue::from_parts(record.record(), record.shape().unwrap_or(&[]))
                            .map_err(|error| extent_error(&error.to_string()))?,
                    );
                    leaves.push(leaf);
                } else {
                    extent_slots.push(leaves.len());
                    leaves.push(TrieLeaf::from_value(&[], MARFValue([0; 40])));
                    values.push(entry.value);
                    encoded.push(prepared);
                }
            }
            if !values.is_empty() {
                let located = store
                    .lock()
                    .map_err(|_| extent_error("extent lock poisoned"))?
                    .append_indexed_encoded(self.marf.sqlite_tx(), &values, Some(&mut encoded))?;
                for (slot, (value, extent)) in extent_slots.into_iter().zip(located) {
                    let mut leaf = TrieLeaf::from_value(&[], value);
                    leaf.extent = Some(extent);
                    leaves[slot] = leaf;
                }
            }
            let _insert = stacks_profiler::diagnostic_span!("Writeback: MARF insert batch");
            stacks_profiler::diagnostics::count("marf_insert_batches", 1);
            return self
                .marf
                .insert_leaf_batch(&keys, leaves)
                .map_err(|error| extent_error(&error.to_string()));
        }

        if !self.value_storage_format.is_binary() {
            return self.put_all_data(
                entries
                    .into_iter()
                    .map(|entry| (entry.key, entry.value.into_canonical()))
                    .collect(),
            );
        }

        let (keys, values) = binary_value_store::put_entries(self.marf.sqlite_tx(), entries)?;
        self.marf
            .insert_batch(&keys, values)
            .map_err(|_| VmInternalError::Expect("ERROR: Unexpected MARF Failure".into()).into())
    }

    fn get_contract_hash(
        &mut self,
        contract: &QualifiedContractIdentifier,
    ) -> Result<(StacksBlockId, Sha512Trunc256Sum), VmExecutionError> {
        sqlite_get_contract_hash(self, contract)
    }

    fn insert_metadata(
        &mut self,
        contract: &QualifiedContractIdentifier,
        key: &str,
        value: &str,
    ) -> Result<(), VmExecutionError> {
        if self.value_storage_format.is_binary() {
            binary_value_store::insert_store_metadata(self, contract, key, value)
        } else {
            sqlite_insert_metadata(self, contract, key, value)
        }
    }

    fn get_metadata(
        &mut self,
        contract: &QualifiedContractIdentifier,
        key: &str,
    ) -> Result<Option<String>, VmExecutionError> {
        if self.value_storage_format.is_binary() {
            binary_value_store::get_store_metadata(self, contract, key)
        } else {
            sqlite_get_metadata(self, contract, key)
        }
    }

    fn get_metadata_manual(
        &mut self,
        at_height: u32,
        contract: &QualifiedContractIdentifier,
        key: &str,
    ) -> Result<Option<String>, VmExecutionError> {
        if self.value_storage_format.is_binary() {
            binary_value_store::get_store_metadata_manual(self, at_height, contract, key)
        } else {
            sqlite_get_metadata_manual(self, at_height, contract, key)
        }
    }
}

impl WritableMarfStore for PersistentWritableMarfStore<'_> {}

/// This trait exists so we can implement `ClarityMarfStore`, `ClarityMarfStoreTransaction`, and
/// `WritableMarfStore` for `Box<dyn WritableMarfStore + '_>`.  We need
/// `Box<dyn WritableMarfStore + '_>` because `dyn WritableMarfStore` doesn't have a size known at
/// compile time (so it cannot be Sized).  But then we'd need it to implement `WritableMarfStore`,
/// which is tricky because some of `ClartyMarfStoreTransaction`'s functions take an instance
/// `self` instead of a reference.  Because we don't know the size of `self` at compile-time, we
/// have to employ a layer of indirection.
///
/// To work around this, `WritableMarfStore` is composed of `BoxedClarityMarfStoreTransaction`
/// below, and we have a blanket implementation of `BoxedClarityMarfStoreTransaction` for any
/// `T: ClarityMarfStoreTransaction`.  This in turn allows us to implement
/// `ClarityMarfStoreTransaction for `Box<dyn WritableMarfStore + 'a>` -- we cast to
/// `ClarityMarfStoreTransaction` to call functions that take a reference to `self`, and we cast to
/// `BoxedClarityMarfStoreTransaction` to call functions that take an instance of `self`.  In the
/// latter case, the instance will have a compile-time size since it will be a Box.  The
/// implementation of `BoxedClarityMarfStoreTransaction` just forwards the call to the
/// corresponding function in `ClarityMarfStoreTransaction` with a reference to the boxed instance.
pub trait BoxedClarityMarfStoreTransaction {
    fn boxed_drop_current_trie(self: Box<Self>);
    fn boxed_drop_unconfirmed(self: Box<Self>) -> Result<(), VmExecutionError>;
    fn boxed_commit_to_processed_block(
        self: Box<Self>,
        target: &StacksBlockId,
    ) -> Result<(), VmExecutionError>;
    fn boxed_commit_to_mined_block(
        self: Box<Self>,
        target: &StacksBlockId,
    ) -> Result<(), VmExecutionError>;
    fn boxed_commit_unconfirmed(self: Box<Self>);

    #[cfg(test)]
    fn boxed_test_commit(self: Box<Self>);
}

impl<T: ClarityMarfStoreTransaction> BoxedClarityMarfStoreTransaction for T {
    fn boxed_drop_current_trie(self: Box<Self>) {
        <Self as ClarityMarfStoreTransaction>::drop_current_trie(*self)
    }

    fn boxed_drop_unconfirmed(self: Box<Self>) -> Result<(), VmExecutionError> {
        <Self as ClarityMarfStoreTransaction>::drop_unconfirmed(*self)
    }

    fn boxed_commit_to_processed_block(
        self: Box<Self>,
        target: &StacksBlockId,
    ) -> Result<(), VmExecutionError> {
        <Self as ClarityMarfStoreTransaction>::commit_to_processed_block(*self, target)
    }

    fn boxed_commit_to_mined_block(
        self: Box<Self>,
        target: &StacksBlockId,
    ) -> Result<(), VmExecutionError> {
        <Self as ClarityMarfStoreTransaction>::commit_to_mined_block(*self, target)
    }

    fn boxed_commit_unconfirmed(self: Box<Self>) {
        <Self as ClarityMarfStoreTransaction>::commit_unconfirmed(*self)
    }

    #[cfg(test)]
    fn boxed_test_commit(self: Box<Self>) {
        <Self as ClarityMarfStoreTransaction>::test_commit(*self)
    }
}

impl<'a> ClarityMarfStoreTransaction for Box<dyn WritableMarfStore + 'a> {
    fn commit_metadata_for_trie(&mut self, target: &StacksBlockId) -> Result<(), VmExecutionError> {
        ClarityMarfStoreTransaction::commit_metadata_for_trie(self.deref_mut(), target)
    }

    fn drop_metadata_for_trie(&mut self, target: &StacksBlockId) -> Result<(), VmExecutionError> {
        ClarityMarfStoreTransaction::drop_metadata_for_trie(self.deref_mut(), target)
    }

    fn seal_trie(&mut self) -> TrieHash {
        ClarityMarfStoreTransaction::seal_trie(self.deref_mut())
    }

    fn drop_current_trie(self) {
        BoxedClarityMarfStoreTransaction::boxed_drop_current_trie(self)
    }

    fn drop_unconfirmed(self) -> Result<(), VmExecutionError> {
        BoxedClarityMarfStoreTransaction::boxed_drop_unconfirmed(self)
    }
    fn commit_to_processed_block(self, target: &StacksBlockId) -> Result<(), VmExecutionError> {
        BoxedClarityMarfStoreTransaction::boxed_commit_to_processed_block(self, target)
    }

    fn commit_to_mined_block(self, target: &StacksBlockId) -> Result<(), VmExecutionError> {
        BoxedClarityMarfStoreTransaction::boxed_commit_to_mined_block(self, target)
    }

    fn commit_unconfirmed(self) {
        BoxedClarityMarfStoreTransaction::boxed_commit_unconfirmed(self)
    }

    #[cfg(test)]
    fn test_commit(self) {
        BoxedClarityMarfStoreTransaction::boxed_test_commit(self)
    }
}

impl<'a> ClarityBackingStore for Box<dyn WritableMarfStore + 'a> {
    fn stores_typed_values(&self) -> bool {
        ClarityBackingStore::stores_typed_values(&**self)
    }

    fn put_all_data(&mut self, items: Vec<(String, String)>) -> Result<(), VmExecutionError> {
        ClarityBackingStore::put_all_data(self.deref_mut(), items)
    }

    fn put_all_data_entries(
        &mut self,
        entries: Vec<DataStoreEntry>,
    ) -> Result<(), VmExecutionError> {
        ClarityBackingStore::put_all_data_entries(self.deref_mut(), entries)
    }

    #[stacks_profiler::profile(name = "Clarity backing read key")]
    fn get_data(&mut self, key: &str) -> Result<Option<String>, VmExecutionError> {
        ClarityBackingStore::get_data(self.deref_mut(), key)
    }

    #[stacks_profiler::profile(name = "Clarity backing read hash")]
    fn get_data_from_path(&mut self, hash: &TrieHash) -> Result<Option<String>, VmExecutionError> {
        ClarityBackingStore::get_data_from_path(self.deref_mut(), hash)
    }

    #[stacks_profiler::profile(name = "Clarity backing typed read")]
    fn get_typed_value(
        &mut self,
        key: &str,
        expected: &TypeSignature,
        epoch: &StacksEpochId,
    ) -> Result<Option<TypedValueResult>, VmExecutionError> {
        ClarityBackingStore::get_typed_value(self.deref_mut(), key, expected, epoch)
    }

    #[stacks_profiler::profile(name = "Clarity backing stored read")]
    fn get_stored_value(
        &mut self,
        key: &str,
        expected: &TypeSignature,
        epoch: &StacksEpochId,
    ) -> Result<Option<StoredValueResult>, VmExecutionError> {
        ClarityBackingStore::get_stored_value(self.deref_mut(), key, expected, epoch)
    }

    fn get_data_with_proof(
        &mut self,
        key: &str,
    ) -> Result<Option<(String, Vec<u8>)>, VmExecutionError> {
        ClarityBackingStore::get_data_with_proof(self.deref_mut(), key)
    }

    fn get_data_with_proof_from_path(
        &mut self,
        hash: &TrieHash,
    ) -> Result<Option<(String, Vec<u8>)>, VmExecutionError> {
        ClarityBackingStore::get_data_with_proof_from_path(self.deref_mut(), hash)
    }

    fn set_block_hash(&mut self, bhh: StacksBlockId) -> Result<StacksBlockId, VmExecutionError> {
        ClarityBackingStore::set_block_hash(self.deref_mut(), bhh)
    }

    fn get_block_at_height(&mut self, height: u32) -> Option<StacksBlockId> {
        ClarityBackingStore::get_block_at_height(self.deref_mut(), height)
    }

    fn get_current_block_height(&mut self) -> u32 {
        ClarityBackingStore::get_current_block_height(self.deref_mut())
    }

    fn get_open_chain_tip_height(&mut self) -> u32 {
        ClarityBackingStore::get_open_chain_tip_height(self.deref_mut())
    }

    fn get_open_chain_tip(&mut self) -> StacksBlockId {
        ClarityBackingStore::get_open_chain_tip(self.deref_mut())
    }

    fn get_side_store(&mut self) -> &Connection {
        ClarityBackingStore::get_side_store(self.deref_mut())
    }

    fn get_cc_special_cases_handler(&self) -> Option<SpecialCaseHandler> {
        ClarityBackingStore::get_cc_special_cases_handler(&**self)
    }

    fn get_contract_hash(
        &mut self,
        contract: &QualifiedContractIdentifier,
    ) -> Result<(StacksBlockId, Sha512Trunc256Sum), VmExecutionError> {
        ClarityBackingStore::get_contract_hash(self.deref_mut(), contract)
    }

    fn insert_metadata(
        &mut self,
        contract: &QualifiedContractIdentifier,
        key: &str,
        value: &str,
    ) -> Result<(), VmExecutionError> {
        ClarityBackingStore::insert_metadata(self.deref_mut(), contract, key, value)
    }

    fn get_metadata(
        &mut self,
        contract: &QualifiedContractIdentifier,
        key: &str,
    ) -> Result<Option<String>, VmExecutionError> {
        ClarityBackingStore::get_metadata(self.deref_mut(), contract, key)
    }

    fn get_metadata_manual(
        &mut self,
        at_height: u32,
        contract: &QualifiedContractIdentifier,
        key: &str,
    ) -> Result<Option<String>, VmExecutionError> {
        ClarityBackingStore::get_metadata_manual(self.deref_mut(), at_height, contract, key)
    }
}

impl<'a> ClarityMarfStore for Box<dyn WritableMarfStore + 'a> {}
impl<'a> WritableMarfStore for Box<dyn WritableMarfStore + 'a> {}

/// Load the explicit completion marker; the presence of a partial extent file never activates it.
fn open_value_extents(
    marf: &mut MARF<StacksBlockId>,
) -> Result<Option<Arc<Mutex<ValueExtentStore>>>, VmExecutionError> {
    let Some(store) =
        ValueExtentStore::open_registered(marf.sqlite_conn(), Path::new(marf.get_db_path()), true)?
    else {
        return Ok(None);
    };
    let store = Arc::new(Mutex::new(store));
    marf.set_value_extent_resolver(store.clone());
    Ok(Some(store))
}

/// Immutable value record obtained from an extent or retained inline trie bytes.
enum LeafValueRecord {
    /// Record in the separate values file.
    Extent(MappedValueRecord),
    /// Record in a trie leaf.
    Inline(InlineValueRecord),
}

impl LeafValueRecord {
    /// Reconstruct the exact canonical string only when requested.
    fn canonical(&self) -> Result<String, VmExecutionError> {
        match self {
            Self::Extent(record) => record.canonical(),
            Self::Inline(record) => record.canonical(),
        }
    }

    /// Project a typed value while retaining the underlying byte owner.
    fn stored(
        &self,
        expected: &TypeSignature,
        epoch: &StacksEpochId,
    ) -> Result<StoredValueResult, VmExecutionError> {
        match self {
            Self::Extent(record) => record.stored(expected, epoch),
            Self::Inline(record) => record.stored(expected, epoch),
        }
    }
}

/// Resolve bytes from the historical leaf reached by traversal, never from the current tip.
fn read_value_extent(
    store: &Arc<Mutex<ValueExtentStore>>,
    leaf: &TrieLeaf,
) -> Result<LeafValueRecord, VmExecutionError> {
    if let Some(inline) = &leaf.inline {
        return Ok(LeafValueRecord::Inline(InlineValueRecord::from_inline(
            inline,
        )));
    }
    let extent = leaf
        .extent
        .ok_or_else(|| extent_error("extent-mode leaf has no value locator"))?;
    store
        .lock()
        .map_err(|_| extent_error("extent lock poisoned"))?
        .read_at(extent)
        .map(LeafValueRecord::Extent)
}

/// Preserve concrete storage errors through the Clarity backing-store API.
fn extent_error(message: &str) -> VmExecutionError {
    VmInternalError::DBError(message.into()).into()
}

#[cfg(test)]
mod extent_tests {
    use std::collections::HashMap;

    use tempfile::tempdir;

    use super::*;
    use crate::chainstate::stacks::index::TrieMerkleProof;

    /// Extent values retain fork history, survive reopen and support unchanged inclusion proofs.
    #[test]
    fn extent_history_rollback_reopen_and_proofs() {
        for compression in [false, true] {
            for mmap in [false, true] {
                extent_history_case(compression, mmap);
            }
        }
    }

    /// Exercise compact leaves through both mapped and positioned I/O, with and without patches.
    fn extent_history_case(compression: bool, mmap: bool) {
        let options = || {
            Some(
                MARFOpenOpts::default()
                    .with_compression(compression)
                    .with_mmap(mmap),
            )
        };
        let directory = tempdir().unwrap();
        let path = directory.path().to_str().unwrap();
        let b1 = StacksBlockId([11; 32]);
        let b2 = StacksBlockId([12; 32]);
        let fork = StacksBlockId([13; 32]);
        let aborted = StacksBlockId([14; 32]);
        let mut marf = MarfedKV::open(path, None, options()).unwrap();
        marf.enable_value_extents().unwrap();
        {
            let mut store = marf.begin(&StacksBlockId::sentinel(), &b1);
            store
                .put_all_data(vec![
                    ("key".into(), "initial".into()),
                    ("inherited".into(), "old".into()),
                ])
                .unwrap();
            store.test_commit();
        }
        let root = marf.get_marf().get_root_hash_at(&b1).unwrap();
        {
            let mut store = marf.begin_read_only(Some(&b1));
            let (value, bytes) = store.get_data_with_proof("key").unwrap().unwrap();
            assert_eq!(value, "initial");
            let proof =
                TrieMerkleProof::<StacksBlockId>::consensus_deserialize(&mut bytes.as_slice())
                    .unwrap();
            assert!(proof.verify(
                &TrieHash::from_key("key"),
                &MARFValue::from_value(&value),
                &root,
                &HashMap::from([(root, b1.clone())])
            ));
            let rows: i64 = store
                .get_side_store()
                .query_row("SELECT COUNT(*) FROM data_table", [], |row| row.get(0))
                .unwrap();
            assert_eq!(rows, 0, "extent writes must not populate SQLite values");
        }
        for (block, value) in [(&b2, "canonical"), (&fork, "fork"), (&aborted, "aborted")] {
            let mut store = marf.begin(&b1, block);
            store
                .put_all_data(vec![("key".into(), value.into())])
                .unwrap();
            assert_eq!(store.get_data("key").unwrap().as_deref(), Some(value));
            if block == &aborted {
                store.drop_current_trie();
            } else {
                store.test_commit();
            }
        }
        drop(marf);
        let mut reopened = MarfedKV::open(path, Some(&b2), options()).unwrap();
        for (block, expected) in [(&b1, "initial"), (&b2, "canonical"), (&fork, "fork")] {
            let mut store = reopened.begin_read_only(Some(block));
            assert_eq!(store.get_data("key").unwrap().as_deref(), Some(expected));
            assert_eq!(
                store
                    .get_data_from_path(&TrieHash::from_key("inherited"))
                    .unwrap()
                    .as_deref(),
                Some("old")
            );
        }
        assert!(!reopened
            .get_marf()
            .with_conn(|conn| conn.has_block(&aborted))
            .unwrap());
        let mut ephemeral = reopened
            .begin_ephemeral(&b2, &StacksBlockId([15; 32]))
            .unwrap();
        assert_eq!(
            ephemeral.get_data("key").unwrap().as_deref(),
            Some("canonical")
        );
        ephemeral
            .put_all_data(vec![("key".into(), "ephemeral".into())])
            .unwrap();
        assert_eq!(
            ephemeral.get_data("key").unwrap().as_deref(),
            Some("ephemeral")
        );
        assert_eq!(
            ephemeral.get_data_with_proof("key").unwrap().unwrap().0,
            "ephemeral"
        );
        let rows: u64 = ephemeral
            .get_side_store()
            .query_row("SELECT COUNT(*) FROM data_table", [], |row| row.get(0))
            .unwrap();
        assert_eq!(rows, 0, "ephemeral writes must not use SQLite values");
        ephemeral.drop_current_trie();
        assert_eq!(
            reopened
                .begin_read_only(Some(&b2))
                .get_data("key")
                .unwrap()
                .as_deref(),
            Some("canonical")
        );
    }

    /// Typed inline and extent writes preserve roots, proofs and values across every type family.
    #[test]
    fn inline_all_types_match_legacy_roots_and_retained_reads() {
        use clarity::vm::database::TypedValueData;
        use clarity::vm::types::{PrincipalData, TupleData, Value};

        let contract = QualifiedContractIdentifier::transient();
        let mut values = vec![
            Value::Int(i128::MIN),
            Value::Int(-1),
            Value::Int(i128::MAX),
            Value::UInt(0),
            Value::UInt(u128::MAX),
            Value::Bool(false),
            Value::Bool(true),
            Value::buff_from(vec![3, 5, 7]).unwrap(),
            Value::string_ascii_from_bytes(b"ascii".to_vec()).unwrap(),
            Value::string_utf8_from_bytes("é中".as_bytes().to_vec()).unwrap(),
            Value::Principal(PrincipalData::Standard(contract.issuer.clone())),
            Value::Principal(PrincipalData::Contract(contract)),
            Value::none(),
            Value::some(Value::UInt(7)).unwrap(),
            Value::okay(Value::Bool(true)).unwrap(),
            Value::error(Value::Int(-1)).unwrap(),
            Value::list_from(vec![Value::UInt(0), Value::UInt(u128::MAX)]).unwrap(),
            Value::list_from(vec![Value::Int(0), Value::Int(i128::MIN)]).unwrap(),
            Value::list_from(vec![Value::Bool(true); 17]).unwrap(),
            Value::buff_from(vec![77; 400]).unwrap(),
        ];
        let tuple = Value::from(
            TupleData::from_data(vec![
                ("a".try_into().unwrap(), Value::Bool(true)),
                ("b".try_into().unwrap(), Value::Bool(false)),
            ])
            .unwrap(),
        );
        values.push(tuple.clone());
        values.push(Value::list_from(vec![tuple.clone(); 4]).unwrap());
        values.push(Value::some(Value::okay(tuple).unwrap()).unwrap());
        for mmap in [false, true] {
            let options = || Some(MARFOpenOpts::default().with_mmap(mmap));
            let before_dir = tempdir().unwrap();
            let after_dir = tempdir().unwrap();
            let mut before =
                MarfedKV::open(before_dir.path().to_str().unwrap(), None, options()).unwrap();
            let mut after =
                MarfedKV::open(after_dir.path().to_str().unwrap(), None, options()).unwrap();
            after.enable_value_extents().unwrap();
            let block = StacksBlockId([41; 32]);
            for marf in [&mut before, &mut after] {
                let mut store = marf.begin(&StacksBlockId::sentinel(), &block);
                let entries = values
                    .iter()
                    .enumerate()
                    .map(|(index, value)| DataStoreEntry {
                        key: format!("type-{index}"),
                        value: DataStoreValue::Typed(
                            TypedValueData::prepare(value.clone()).unwrap(),
                        ),
                    })
                    .collect();
                store.put_all_data_entries(entries).unwrap();
                store
                    .put_all_data(
                        (0..300)
                            .map(|n| (format!("padding-{n}"), "small".into()))
                            .collect(),
                    )
                    .unwrap();
                store.test_commit();
            }
            assert_eq!(
                before.get_marf().get_root_hash_at(&block).unwrap(),
                after.get_marf().get_root_hash_at(&block).unwrap()
            );
            drop(after);
            let mut after =
                MarfedKV::open(after_dir.path().to_str().unwrap(), Some(&block), options())
                    .unwrap();
            let mut retained = Vec::new();
            let mut inline_count = 0;
            let mut mapped_projections = 0;
            let mut extent_count = 0;
            for (index, value) in values.iter().enumerate() {
                let key = format!("type-{index}");
                let leaf = after
                    .get_marf()
                    .get_leaf_by_key(&block, &key)
                    .unwrap()
                    .unwrap();
                inline_count += usize::from(leaf.inline.is_some());
                extent_count += usize::from(leaf.extent.is_some());
                let expected = TypeSignature::type_of(value).unwrap();
                let stored = after
                    .begin_read_only(Some(&block))
                    .get_stored_value(&key, &expected, &StacksEpochId::latest())
                    .unwrap()
                    .unwrap();
                if let (Some(inline), StoredValue::Packed(projected)) =
                    (&leaf.inline, &stored.value)
                {
                    if inline.is_mapped() && (7..=9).contains(&index) {
                        let address =
                            projected.as_view().as_sequence_bytes().unwrap().as_ptr() as usize;
                        let start = inline.record().as_ptr() as usize;
                        assert!(
                            (start..start + inline.record().len()).contains(&address),
                            "VM bytes must alias the inline trie payload"
                        );
                        mapped_projections += 1;
                    }
                }
                retained.push((stored.value, value.clone()));
                let old_proof = before
                    .begin_read_only(Some(&block))
                    .get_data_with_proof(&key)
                    .unwrap();
                let new_proof = after
                    .begin_read_only(Some(&block))
                    .get_data_with_proof(&key)
                    .unwrap();
                assert_eq!(new_proof, old_proof);
            }
            assert!(inline_count > 10);
            assert!(extent_count > 0);
            if mmap {
                assert!(mapped_projections > 0);
                assert!(
                    (0..300).any(|n| after
                        .get_marf()
                        .get_leaf_by_key(&block, &format!("padding-{n}"))
                        .unwrap()
                        .unwrap()
                        .inline
                        .unwrap()
                        .is_mapped()),
                    "persisted inline reads must retain the trie mapping"
                );
            }
            let rows: i64 = after
                .marf
                .sqlite_conn()
                .query_row("SELECT COUNT(*) FROM clarity_extent_index", [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(
                rows as usize, extent_count,
                "inline writes must bypass the dedup index"
            );
            let next = StacksBlockId([42; 32]);
            {
                let mut store = after.begin(&block, &next);
                store
                    .put_all_data(
                        (0..300)
                            .map(|n| (format!("padding-{n}"), "replacement".into()))
                            .collect(),
                    )
                    .unwrap();
                store.test_commit();
            }
            drop(after);
            for (stored, expected) in retained {
                match stored {
                    StoredValue::Owned(actual) => assert_eq!(actual, expected),
                    StoredValue::Packed(actual) => {
                        assert_eq!(actual.to_owned_value().unwrap(), expected)
                    }
                }
            }
        }
    }
}
