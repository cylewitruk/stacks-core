//! Immutable canonical value views and the Clarity stable-store adapter.

use std::io;
use std::ops::Range;
use std::path::{Component, Path};
use std::sync::{Arc, Mutex};

use clarity::vm::database::{DataStoreValue, StoredValue, StoredValueResult};
use clarity::vm::errors::{VmExecutionError, VmInternalError};
use clarity::vm::types::codec::packed::{
    PackedByteOwner, PackedValueError, PackedValueRef, SharedPackedValue,
};
use clarity::vm::types::{TypeSignature, Value};
use rusqlite::Connection;
use stable_value_format::ValueId;
use stacks_common::types::StacksEpochId;
use stacks_common::util::hash::{hex_bytes, to_hex};

use super::binary_value_store::{self, EncodedRecord};
use super::stable_value_store::StableValueStore;
use crate::chainstate::stacks::index::inline_value::{InlineValue, InlineValueBytes};
use crate::chainstate::stacks::index::{Error as IndexError, MARFValue, ValueResolver};

/// A checked record borrowing its immutable mapping.
#[derive(Debug)]
pub struct ExtentValueRecord<O: PackedByteOwner, D: PackedByteOwner = O> {
    /// Owner retained by projected packed values.
    mapping: Arc<O>,
    /// Descriptor owner, separate from the record owner for stable-ID values.
    descriptor_mapping: Arc<D>,
    /// Binary V1 envelope and value payload within the owner.
    record: Range<usize>,
    /// Independently versioned reconstruction descriptor within its owner.
    descriptor: Range<usize>,
}

/// Physical reference returned by the canonical Clarity value store.
pub enum ExternalValueRef {
    /// Stable four-byte stable value identifier.
    Stable(ValueId),
}

/// Canonical external value storage.
pub enum ValueBackend {
    /// Partitioned stable-ID generation.
    Stable(StableValueStore),
}

impl ValueBackend {
    /// Open only a completed canonical generation with matching SQLite access.
    pub fn open_registered(
        db: &Connection,
        db_path: &Path,
    ) -> Result<Option<Self>, VmExecutionError> {
        let present: bool = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='clarity_stable_format')",
            [],
            |row| row.get(0),
        ).map_err(|error| storage_error(&error.to_string()))?;
        if present {
            let (name, id): (String, Vec<u8>) = db.query_row(
                "SELECT path,store_id FROM clarity_stable_format WHERE singleton=1 AND version=1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            ).map_err(|error| storage_error(&error.to_string()))?;
            let component = Path::new(&name);
            if component.components().count() != 1
                || !matches!(component.components().next(), Some(Component::Normal(_)))
            {
                return Err(storage_error(
                    "Stable generation must be a sibling directory",
                ));
            }
            let store_id: [u8; 16] = id
                .try_into()
                .map_err(|_| storage_error("Invalid stable generation UUID"))?;
            let parent = db_path
                .canonicalize()
                .map_err(io_error)?
                .parent()
                .ok_or_else(|| storage_error("Clarity database has no parent"))?
                .to_path_buf();
            let store = if db
                .is_readonly(rusqlite::DatabaseName::Main)
                .map_err(|error| storage_error(&error.to_string()))?
            {
                StableValueStore::open_readonly(&parent.join(component), store_id, db)?
            } else {
                StableValueStore::recover_open(&parent.join(component), store_id, db)?
            };
            return Ok(Some(Self::Stable(store)));
        }
        let retired: bool = db
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='clarity_extent_format')",
                [],
                |row| row.get(0),
            )
            .map_err(|error| storage_error(&error.to_string()))?;
        if retired {
            return Err(storage_error(
                "Retired direct-extent generation requires its frozen tooling",
            ));
        }
        Ok(None)
    }

    /// Keep the selected backend ready for one new MARF block.
    pub fn begin_block(&mut self) -> Result<(), VmExecutionError> {
        match self {
            Self::Stable(store) => store.begin_block(),
        }
    }

    /// Synchronize value bytes and directories before MARF references commit.
    pub fn publish_block(&mut self) -> Result<(), VmExecutionError> {
        match self {
            Self::Stable(store) => store.sync_unpublished(),
        }
    }

    /// Refresh a direct extent mapping; stable reads open checked files by ID.
    pub fn refresh_mapping(&mut self) -> Result<(), VmExecutionError> {
        match self {
            Self::Stable(store) => store.refresh_if_changed(false),
        }
    }

    /// Release transaction-local state after an aborted block.
    pub fn discard_block(&mut self) {
        match self {
            Self::Stable(store) => store.discard_unpublished_cache(),
        }
    }

    /// Admit cache entries only after the MARF transaction commits.
    pub fn commit_dedup_cache(&mut self) {
        match self {
            Self::Stable(store) => store.commit_dedup_cache(),
        }
    }

    /// Write one batch in the caller's MARF SQLite transaction.
    pub fn append_indexed_encoded(
        &mut self,
        db: &Connection,
        values: &[DataStoreValue],
        encoded: Option<&mut [Option<EncodedRecord>]>,
    ) -> Result<Vec<(MARFValue, ExternalValueRef)>, VmExecutionError> {
        match self {
            Self::Stable(store) => store
                .append_indexed_encoded(db, values, encoded)
                .map(|items| {
                    items
                        .into_iter()
                        .map(|(hash, id)| (hash, ExternalValueRef::Stable(id)))
                        .collect()
                }),
        }
    }

    /// Resolve one stable ID while retaining its packed payload bytes.
    pub fn read_id(&mut self, id: ValueId) -> Result<StableMappedValueRecord, VmExecutionError> {
        let Self::Stable(store) = self;
        let value = store.read(id)?;
        Ok(StableMappedValueRecord::from_stable_parts(value))
    }

    /// Check whether this activated backend uses stable IDs.
    pub fn is_stable(&self) -> bool {
        matches!(self, Self::Stable(_))
    }
}

impl ValueResolver for Mutex<ValueBackend> {
    fn inline_commitment(&self, value: &InlineValue) -> Result<MARFValue, IndexError> {
        InlineValueRecord::from_inline(value)
            .commitment()
            .map_err(|error| IndexError::CorruptionError(error.to_string()))
    }

    fn commitment_by_id(&self, raw: u32) -> Result<MARFValue, IndexError> {
        let id =
            ValueId::new(raw).map_err(|error| IndexError::CorruptionError(error.to_string()))?;
        let mut backend = self
            .lock()
            .map_err(|_| IndexError::CorruptionError("Value backend lock poisoned".into()))?;
        let ValueBackend::Stable(store) = &mut *backend;
        store
            .commitment(id)
            .map_err(|error| IndexError::CorruptionError(error.to_string()))
    }
}

/// Value record retained entirely in RAM for ephemeral execution.
pub type OwnedValueRecord = ExtentValueRecord<Vec<u8>>;
/// Stable-ID record retaining value pages and separately interned descriptor bytes.
pub type StableMappedValueRecord =
    ExtentValueRecord<super::stable_value_store::StablePartitionBytes, Vec<u8>>;
/// Inline record retaining the trie mapping or its pending/fallback byte owner.
pub type InlineValueRecord = ExtentValueRecord<InlineValueBytes>;

impl InlineValueRecord {
    /// Reuse the extent decoder over the inline record's immutable owner and checked ranges.
    pub fn from_inline(value: &InlineValue) -> Self {
        let owner = value.owner();
        Self {
            mapping: Arc::clone(&owner),
            descriptor_mapping: owner,
            record: value.record_range(),
            descriptor: value.descriptor_range(),
        }
    }

    /// Reconstruct the logical commitment only when a generic MARF/hash/proof read needs it.
    pub fn commitment(&self) -> Result<MARFValue, VmExecutionError> {
        self.canonical()
            .map(|canonical| MARFValue::from_value(&canonical))
    }
}

impl OwnedValueRecord {
    /// Encode one ephemeral value without creating a SQLite value row.
    pub fn from_value(value: &DataStoreValue) -> Result<Self, VmExecutionError> {
        let encoded = binary_value_store::encode_entry(value)?;
        let mut bytes = encoded.record().to_vec();
        let record_end = bytes.len();
        bytes.extend_from_slice(encoded.shape().unwrap_or_default());
        let end = bytes.len();
        let owner = Arc::new(bytes);
        Ok(Self {
            mapping: Arc::clone(&owner),
            descriptor_mapping: owner,
            record: 0..record_end,
            descriptor: record_end..end,
        })
    }
}

impl StableMappedValueRecord {
    /// Retain mapped value bytes and a separate exact descriptor without copying the value.
    pub fn from_stable_parts(value: super::stable_value_store::StableValueRecord) -> Self {
        let descriptor_end = value.descriptor.len();
        Self {
            mapping: value.owner,
            descriptor_mapping: value.descriptor,
            record: value.record,
            descriptor: 0..descriptor_end,
        }
    }
}

impl<O: PackedByteOwner + 'static, D: PackedByteOwner> ExtentValueRecord<O, D> {
    /// Reconstruct exact canonical text for generic backing-store consumers.
    pub fn canonical(&self) -> Result<String, VmExecutionError> {
        let record = &self.mapping.as_ref().as_ref()[self.record.clone()];
        let descriptor = &self.descriptor_mapping.as_ref().as_ref()[self.descriptor.clone()];
        canonical_from_encoded_parts(record, descriptor)
    }

    /// Return a mapped packed value, with historical schema projection handled by owned fallback.
    pub fn stored(
        &self,
        expected: &TypeSignature,
        epoch: &StacksEpochId,
    ) -> Result<StoredValueResult, VmExecutionError> {
        let record = &self.mapping.as_ref().as_ref()[self.record.clone()];
        if record.starts_with(&[1, 2])
            && PackedValueRef::parse(&record[2..])
                .and_then(|packed| {
                    packed.matches_storage_schema(
                        &self.descriptor_mapping.as_ref().as_ref()[self.descriptor.clone()],
                        expected,
                    )
                })
                .map_err(|error| storage_error(&error.to_string()))?
        {
            // The encoder/migrator admitted these immutable bytes. The VM supplies the declared
            // storage type; differing historical shapes use canonical projection below.
            match SharedPackedValue::from_encoded_owner(
                Arc::clone(&self.mapping),
                self.record.start + 2..self.record.end,
                expected,
                epoch,
            ) {
                Ok(value) => {
                    return Ok(StoredValueResult {
                        serialized_byte_len: u64::from(value.consensus_byte_len()),
                        #[cfg(not(feature = "direct-value-eager"))]
                        value: StoredValue::Packed(value),
                        #[cfg(feature = "direct-value-eager")]
                        value: StoredValue::Owned(
                            value
                                .to_owned_value()
                                .map_err(|error| storage_error(&error.to_string()))?,
                        ),
                    });
                }
                Err(PackedValueError::Invariant(error)) => {
                    return Err(storage_error(&error.to_string()));
                }
                Err(_) => {}
            }
        }
        let canonical = self.canonical()?;
        let bytes = hex_bytes(&canonical)
            .map_err(|_| storage_error("typed extent is not consensus bytes"))?;
        let value = Value::try_deserialize_bytes_at_epoch(&bytes, expected, epoch)
            .map_err(|error| storage_error(&error.to_string()))?;
        Ok(StoredValueResult {
            serialized_byte_len: bytes.len() as u64,
            value: StoredValue::Owned(value),
        })
    }
}

/// Decode an existing Binary V1 payload and descriptor using the ordinary read semantics.
pub(crate) fn canonical_from_encoded_parts(
    record: &[u8],
    descriptor: &[u8],
) -> Result<String, VmExecutionError> {
    let Some((&kind, payload)) = record
        .strip_prefix(&[1])
        .and_then(|record| record.split_first())
    else {
        return Err(storage_error("invalid value extent envelope"));
    };
    match kind {
        0 => String::from_utf8(payload.to_vec())
            .map_err(|_| storage_error("invalid canonical UTF-8")),
        1 => Ok(to_hex(payload)),
        2 => PackedValueRef::parse(payload)
            .and_then(|packed| packed.reconstruct_consensus(descriptor))
            .map(|consensus| to_hex(&consensus))
            .map_err(|error| storage_error(&error.to_string())),
        _ => Err(storage_error("unknown value extent kind")),
    }
}

/// Translate filesystem errors to backing-store failures.
fn io_error(error: io::Error) -> VmExecutionError {
    storage_error(&error.to_string())
}

/// Construct a storage failure without accepting a partially read value.
fn storage_error(message: &str) -> VmExecutionError {
    VmInternalError::DBError(message.into()).into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chainstate::stacks::index::FileMapping;
    use clarity::vm::database::TypedValueData;
    use std::io::{Seek, SeekFrom, Write};

    /// Inline generic reads retain exact canonical spelling and the original commitment.
    #[test]
    fn inline_record_reuses_canonical_reconstruction() {
        for canonical in [
            "",
            "metadata",
            "00FF",
            "03",
            "0100000000000000000000000000000001",
        ] {
            let value = DataStoreValue::Canonical(canonical.into());
            let encoded = binary_value_store::encode_entry(&value).unwrap();
            let inline =
                InlineValue::from_parts(encoded.record(), encoded.shape().unwrap_or_default())
                    .unwrap();
            let record = InlineValueRecord::from_inline(&inline);
            assert_eq!(record.canonical().unwrap(), canonical);
            assert_eq!(
                record.commitment().unwrap(),
                MARFValue::from_value(canonical)
            );
        }
    }

    /// A VM projection retains the original mapped payload after every read handle is dropped.
    #[cfg(not(feature = "direct-value-eager"))]
    #[test]
    fn inline_vm_projection_retains_original_mapping() {
        let expected_value = Value::buff_from(vec![3, 5, 7]).unwrap();
        let expected = TypeSignature::type_of(&expected_value).unwrap();
        let value = DataStoreValue::Typed(TypedValueData::prepare(expected_value.clone()).unwrap());
        let encoded = binary_value_store::encode_entry(&value).unwrap();
        let descriptor = encoded.shape().unwrap_or_default();
        assert!(InlineValue::fits_inline(
            encoded.record().len(),
            descriptor.len()
        ));
        let mut file = tempfile::tempfile().unwrap();
        file.set_len(128 * 1024).unwrap();
        file.seek(SeekFrom::Start(64)).unwrap();
        file.write_all(encoded.record()).unwrap();
        file.write_all(descriptor).unwrap();
        file.flush().unwrap();
        // SAFETY: The mapped file remains immutable for the lifetime of every retained view.
        let mapping = unsafe { FileMapping::map(&file).unwrap() };
        let length = encoded.record().len() + descriptor.len();
        let range_start = mapping[64..].as_ptr() as usize;
        let inline = InlineValue::from_mapping(
            mapping.clone(),
            64..64 + length,
            encoded.record().len() as u8,
        )
        .unwrap();
        let record = InlineValueRecord::from_inline(&inline);
        assert_eq!(
            record.commitment().unwrap(),
            MARFValue::from_value(value.canonical())
        );
        let stored = record.stored(&expected, &StacksEpochId::latest()).unwrap();
        let StoredValue::Packed(retained) = stored.value else {
            panic!("expected borrowed inline value")
        };
        let address = retained.as_view().as_sequence_bytes().unwrap().as_ptr() as usize;
        assert!((range_start..range_start + encoded.record().len()).contains(&address));
        drop(record);
        drop(inline);
        drop(mapping);
        drop(file);
        assert_eq!(
            retained.as_view().as_sequence_bytes().unwrap().as_ptr() as usize,
            address
        );
        assert_eq!(retained.to_owned_value().unwrap(), expected_value);
    }
}
