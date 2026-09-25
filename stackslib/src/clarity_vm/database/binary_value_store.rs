// Copyright (C) 2026 Stacks Open Internet Foundation
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

//! Production Binary V1 storage for the Clarity-owned SQLite side store.
//!
//! Binary V1 covers the database schema, binary keys, record envelopes, and
//! metadata representation. Typed Clarity `Value` payloads use the separate
//! canonical packed codec in [`clarity::vm::types::codec::packed`].

#![deny(missing_docs)]

mod metadata;
mod migration;
mod schema;

use std::collections::HashMap;
use std::{debug_assert_matches, str};

use clarity::vm::database::{
    ClarityBackingStore, DataStoreEntry, DataStoreValue, SqliteConnection, StoredValue,
    StoredValueResult, TypedValueData, TypedValueResult,
};
use clarity::vm::errors::{RuntimeError, VmExecutionError, VmInternalError};
use clarity::vm::types::codec::packed::{
    PackedCodecInvariant, PackedValue, PackedValueError, PackedValueRef, PackedValueVersion,
    SharedPackedValue, ValueDescriptor, ValueDescriptorRef, ValueDescriptorVersion,
};
use clarity::vm::types::{PrincipalData, QualifiedContractIdentifier, TypeSignature, Value};
use rusqlite::types::ValueRef;
use rusqlite::{Connection, OptionalExtension, params};
use stacks_common::types::StacksEpochId;
use stacks_common::util::hash::{hex_bytes, to_hex};

pub use self::metadata::{MetadataBlockId, MetadataRow};
pub use self::migration::{
    DEFAULT_CACHE_MIB, DataMigrationStats, IntegrityLevel, MAX_CACHE_MIB, MigrationConfig,
    MigrationEvent, MigrationMode, MigrationPhase, MigrationReport, ShapeCacheStats, migrate,
    prepare_extent_destination,
};
use self::schema::{
    AUDIT_DATA, FORMAT_TABLE, GET_GENERIC, GET_SHAPE_ID, GET_TYPED, INSERT_DATA, INSERT_SHAPE,
    RECORD_VERSION,
};
use crate::chainstate::stacks::index::MARFValue;

/// Maximum number of descriptor-to-ID mappings retained by one writer cache.
const SHAPE_CACHE_CAPACITY: usize = 65_536;
/// Maximum descriptor bytes retained by one writer cache.
const SHAPE_CACHE_BYTES: usize = 16 * 1024 * 1024;

/// Exact canonical UTF-8 for values whose logical representation is textual.
const KIND_CANONICAL_UTF8: u8 = 0;
/// Exact lowercase hexadecimal canonical text represented as decoded bytes.
const KIND_CANONICAL_HEX_BYTES: u8 = 1;
/// Canonical Binary V1 packed Clarity value bytes.
const KIND_CANONICAL_PACKED: u8 = 2;

/// Packed value-record version written by Binary V1 storage.
const PACKED_VALUE_CODEC_VERSION: PackedValueVersion = PackedValueVersion::V1;
/// Value-shape descriptor version written by Binary V1 storage.
const VALUE_SHAPE_CODEC_VERSION: ValueDescriptorVersion = ValueDescriptorVersion::V1;

/// Immutable physical format selected when a Clarity side store opens.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ValueStorageFormat {
    /// Current text-keyed and text-valued side store.
    LegacyText,
    /// Completed Binary V1 BLOB storage.
    BinaryV1,
}

impl ValueStorageFormat {
    /// Whether this database uses Binary V1 storage and typed-value operations.
    pub fn is_binary(self) -> bool {
        self == Self::BinaryV1
    }
}

/// One complete Binary V1 record and its optional normalized descriptor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EncodedRecord {
    /// Versioned record envelope stored in `data_table.value`.
    record: Vec<u8>,
    /// Canonical active-value descriptor required by packed generic reads.
    shape: Option<ValueDescriptor>,
}

/// Bounded cache of validated descriptors and their database-local IDs.
#[derive(Debug, Default)]
struct ShapeCache {
    /// Descriptor-to-database-ID mappings safe for the cache's transaction lifetime.
    ids: HashMap<Vec<u8>, i64>,
    /// Descriptor payload bytes currently charged to `ids`.
    bytes: usize,
    /// Number of descriptors resolved without SQLite work.
    hits: u64,
    /// Number of descriptors requiring a dictionary lookup or insert.
    misses: u64,
    /// Number of cached mappings discarded by bounded-cache eviction.
    evictions: u64,
}

impl ShapeCache {
    /// Resolve a validated shape through this cache and the normalized SQLite dictionary.
    fn intern(
        &mut self,
        conn: &Connection,
        shape: &ValueDescriptor,
    ) -> Result<i64, VmExecutionError> {
        let descriptor = shape.as_bytes();
        if let Some(id) = self.ids.get(descriptor) {
            self.hits += 1;
            return Ok(*id);
        }
        let id = intern_shape(conn, descriptor)?;
        if descriptor.len() > SHAPE_CACHE_BYTES {
            self.misses += 1;
            return Ok(id);
        }
        if self.ids.len() >= SHAPE_CACHE_CAPACITY
            || self.bytes.saturating_add(descriptor.len()) > SHAPE_CACHE_BYTES
        {
            self.evictions += self.ids.len() as u64;
            self.ids.clear();
            self.bytes = 0;
        }
        self.ids.insert(descriptor.to_vec(), id);
        self.bytes += descriptor.len();
        self.misses += 1;
        Ok(id)
    }

    /// Return `(cache hits, cache misses, entries evicted)` for diagnostics.
    fn stats(&self) -> (u64, u64, u64) {
        (self.hits, self.misses, self.evictions)
    }
}

/// Shape dictionary cache used while streaming an offline migration.
///
/// The writer may span successfully committed batches. If its transaction is
/// rolled back, discard the writer so cached IDs from that transaction cannot
/// be reused.
#[derive(Debug, Default)]
pub struct MigrationWriter {
    /// Bounded descriptor cache spanning committed migration batches.
    shapes: ShapeCache,
}

const INSERT_MIGRATION_DATA: &str = "INSERT INTO data_table
    (key, value, value_shape_id) VALUES (?1, ?2, ?3)";

impl MigrationWriter {
    /// Create an empty migration writer.
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert one preclassified row while caching normalized shape IDs.
    ///
    /// The caller must discard this writer if the surrounding transaction is
    /// rolled back.
    pub fn put(
        &mut self,
        conn: &Connection,
        value_hash: &MARFValue,
        canonical: &str,
        encoded: &EncodedRecord,
    ) -> Result<(), VmExecutionError> {
        if MARFValue::from_value(canonical) != *value_hash {
            return Err(storage_error(
                "canonical value does not reproduce its data-table key",
            ));
        }
        let shape_id = encoded
            .shape
            .as_ref()
            .map(|shape| self.shapes.intern(conn, shape))
            .transpose()?;
        conn.prepare_cached(INSERT_MIGRATION_DATA)
            .and_then(|mut statement| {
                statement.execute(params![value_hash.as_bytes(), encoded.record(), shape_id])
            })
            .map_err(sql_error)?;
        Ok(())
    }

    /// Return `(cache hits, cache misses, entries evicted)` for diagnostics.
    pub fn shape_cache_stats(&self) -> (u64, u64, u64) {
        self.shapes.stats()
    }
}

impl EncodedRecord {
    /// Borrow the complete versioned record envelope.
    pub fn record(&self) -> &[u8] {
        &self.record
    }

    /// Borrow the normalized value-shape descriptor, when required.
    pub fn shape(&self) -> Option<&[u8]> {
        self.shape.as_ref().map(ValueDescriptor::as_bytes)
    }
}

/// Store one block's logical edits while reusing content hashes and normalized shapes.
///
/// Duplicate hashes are skipped only after their canonical strings compare equal, preserving the
/// collision detection performed by row reconciliation without retaining additional payload copies.
pub fn put_entries(
    conn: &Connection,
    entries: Vec<DataStoreEntry>,
) -> Result<(Vec<String>, Vec<MARFValue>), VmExecutionError> {
    let mut first_by_hash = HashMap::<MARFValue, usize>::with_capacity(entries.len());
    let mut hashed = Vec::with_capacity(entries.len());
    for (index, entry) in entries.iter().enumerate() {
        let value_hash = MARFValue::from_value(entry.value.canonical());
        let is_first = match first_by_hash.get(&value_hash) {
            Some(first) => {
                let first_entry = entries
                    .get(*first)
                    .ok_or_else(|| storage_error("invalid Binary V1 batch hash index"))?;
                if first_entry.value.canonical() != entry.value.canonical() {
                    return Err(storage_error(
                        "distinct canonical values produced one MARF value hash",
                    ));
                }
                false
            }
            None => {
                first_by_hash.insert(value_hash.clone(), index);
                true
            }
        };
        hashed.push((value_hash, is_first));
    }

    let mut shapes = ShapeCache::default();
    let mut keys = Vec::with_capacity(entries.len());
    let mut values = Vec::with_capacity(entries.len());
    for (entry, (value_hash, is_first)) in entries.into_iter().zip(hashed) {
        if is_first {
            match entry.value {
                DataStoreValue::Canonical(canonical) => {
                    put_generic(conn, &value_hash, &canonical, &mut shapes)?;
                }
                DataStoreValue::Typed(typed) => {
                    put_typed(conn, &value_hash, typed, &mut shapes)?;
                }
            }
        }
        keys.push(entry.key);
        values.push(value_hash);
    }
    Ok((keys, values))
}

/// Detect and fully validate the immutable side-store format.
pub fn detect(conn: &Connection) -> Result<ValueStorageFormat, VmExecutionError> {
    if !schema::table_exists(conn, FORMAT_TABLE)? {
        SqliteConnection::check_schema(conn)?;
        return Ok(ValueStorageFormat::LegacyText);
    }
    verify_complete(conn)?;
    Ok(ValueStorageFormat::BinaryV1)
}

/// Replace empty legacy Clarity tables with a completed Binary V1 schema.
pub fn initialize_empty(conn: &Connection) -> Result<(), VmExecutionError> {
    if schema::table_exists(conn, FORMAT_TABLE)? {
        return verify_complete(conn);
    }
    let data_rows = schema::row_count(conn, schema::DATA_TABLE)?;
    let metadata_rows = schema::row_count(conn, schema::METADATA_TABLE)?;
    if data_rows != 0 || metadata_rows != 0 {
        return Err(storage_error(
            "refusing to replace populated legacy Clarity tables with Binary V1",
        ));
    }
    conn.execute("DROP TABLE data_table", [])
        .map_err(sql_error)?;
    conn.execute("DROP TABLE metadata_table", [])
        .map_err(sql_error)?;
    schema::create(conn, true)?;
    verify_complete(conn)
}

/// Create an empty, incomplete Binary V1 destination for offline migration.
pub fn initialize_migration_destination(conn: &Connection) -> Result<(), VmExecutionError> {
    if schema::table_exists(conn, schema::DATA_TABLE)?
        || schema::table_exists(conn, schema::METADATA_TABLE)?
        || schema::table_exists(conn, FORMAT_TABLE)?
        || schema::table_exists(conn, schema::SHAPE_TABLE)?
    {
        return Err(storage_error(
            "Binary V1 migration destination already contains Clarity tables",
        ));
    }
    schema::create(conn, false)
}

/// Build the Binary V1 indexes after an offline destination has been populated.
pub fn create_migration_indexes(conn: &Connection) -> Result<(), VmExecutionError> {
    schema::create_indexes(conn)
}

/// Verify that an incomplete offline destination is ready for publication.
pub fn verify_finalization_ready(conn: &Connection) -> Result<(), VmExecutionError> {
    schema::verify(conn, false, true)
}

/// Mark a fully populated offline destination complete and verify its schema.
pub fn finalize_migration_destination(conn: &Connection) -> Result<(), VmExecutionError> {
    schema::finalize(conn)
}

/// Verify a completed Binary V1 marker and physical table contract.
pub fn verify_complete(conn: &Connection) -> Result<(), VmExecutionError> {
    schema::verify(conn, true, true)
}

/// Return every Clarity-owned Binary V1 table name.
pub fn table_names() -> &'static [&'static str] {
    schema::TABLE_NAMES
}

/// Return the Binary V1 content-addressed value table name.
pub fn data_table_name() -> &'static str {
    schema::DATA_TABLE
}

/// Return the Binary V1 metadata table name.
pub fn metadata_table_name() -> &'static str {
    schema::METADATA_TABLE
}

/// Return the data-row column containing a database-local shape identifier.
pub fn data_shape_id_column() -> &'static str {
    schema::VALUE_SHAPE_ID_COLUMN
}

/// Whether a table belongs to the Binary V1 Clarity side-store schema.
pub fn owns_table(name: &str) -> bool {
    schema::owns_table(name)
}

/// Copy Binary V1 dictionary and format rows from the attached `src` database.
pub fn copy_snapshot_auxiliary_rows(conn: &Connection) -> Result<(), VmExecutionError> {
    schema::copy_snapshot_auxiliary_rows(conn)
}

/// Count Binary V1 content-addressed value rows.
pub fn data_row_count(conn: &Connection) -> Result<u64, VmExecutionError> {
    schema::row_count(conn, schema::DATA_TABLE)
}

/// Count Binary V1 metadata rows.
pub fn metadata_row_count(conn: &Connection) -> Result<u64, VmExecutionError> {
    metadata::row_count(conn).map_err(sql_error)
}

/// Verify all normalized shape references resolve in this database.
pub fn verify_shape_references(conn: &Connection) -> Result<(), VmExecutionError> {
    schema::verify_shape_references(conn)
}

/// Exhaustively audit every Binary V1 record and report progress to `visit_count`.
pub fn audit_all_records<F>(conn: &Connection, mut visit_count: F) -> Result<u64, VmExecutionError>
where
    F: FnMut(u64),
{
    let mut statement = conn.prepare(AUDIT_DATA).map_err(sql_error)?;
    let mut rows = statement.query([]).map_err(sql_error)?;
    let mut count = 0;
    while let Some(row) = rows.next().map_err(sql_error)? {
        let value_hash = MARFValue(
            row.get_ref(0)
                .map_err(sql_error)?
                .as_blob()
                .map_err(codec_error)?
                .try_into()
                .map_err(|_| storage_error("invalid Binary V1 key length during audit"))?,
        );
        let record = row
            .get_ref(1)
            .map_err(sql_error)?
            .as_blob()
            .map_err(codec_error)?;
        let shape = match row.get_ref(2).map_err(sql_error)? {
            ValueRef::Null => None,
            ValueRef::Blob(value) => Some(value),
            value => {
                return Err(storage_error(format!(
                    "invalid Binary V1 shape storage class during audit: {:?}",
                    value.data_type()
                )));
            }
        };
        audit_stored_record(&value_hash, record, shape)?;
        count += 1;
        visit_count(count);
    }
    Ok(count)
}

/// Encode and store one prepared typed Clarity value under its precomputed logical hash.
fn put_typed(
    conn: &Connection,
    value_hash: &MARFValue,
    typed: TypedValueData,
    shapes: &mut ShapeCache,
) -> Result<(), VmExecutionError> {
    let encoded = encode_typed(&typed)?;
    insert_or_reconcile(conn, value_hash, typed.canonical(), &encoded, shapes)
}

/// Encode a candidate for inline storage, skipping provably oversized values.
/// Returned oversized records may still be reused by the extent writer on a dedup miss.
pub fn encode_inline_candidate(
    value: &DataStoreValue,
    limit: usize,
) -> Result<Option<EncodedRecord>, VmExecutionError> {
    if let DataStoreValue::Canonical(text) = value {
        let definitely_text = text.len() % 2 != 0
            || text
                .as_bytes()
                .iter()
                .take(64)
                .any(|byte| !byte.is_ascii_digit() && !(b'a'..=b'f').contains(byte));
        if definitely_text && text.len().checked_add(2).is_none_or(|bytes| bytes > limit) {
            return Ok(None);
        }
    }
    if let DataStoreValue::Typed(typed) = value {
        if let Some(owned) = typed.owned_value() {
            let bytes = PackedValue::encoded_byte_len(PACKED_VALUE_CODEC_VERSION, owned)
                .map_err(codec_error)?;
            if bytes.checked_add(2).is_none_or(|bytes| bytes > limit) {
                return Ok(None);
            }
        } else if let Some(shared) = typed.shared_value() {
            let prefix = 2 + PACKED_VALUE_CODEC_VERSION.header_len();
            if limit < prefix {
                return Ok(None);
            }
            let body_limit = limit - prefix;
            let mut visits = 64;
            let bytes = shared
                .as_view()
                .body_len_lower_bound(body_limit.saturating_add(1), &mut visits)
                .map_err(codec_error)?;
            if bytes > body_limit {
                return Ok(None);
            }
        }
    }
    encode_entry(value).map(Some)
}

/// Encode a prepared write without decoding its canonical representation again.
pub fn encode_entry(value: &DataStoreValue) -> Result<EncodedRecord, VmExecutionError> {
    match value {
        DataStoreValue::Canonical(canonical) => encode_migrated(canonical),
        DataStoreValue::Typed(typed) => encode_typed(&typed),
    }
}

/// Encode a shared write from its already-produced consensus bytes without owned decoding.
fn encode_typed(typed: &TypedValueData) -> Result<EncodedRecord, VmExecutionError> {
    if let Some(consensus) = typed.shared_consensus() {
        let (packed, shape) = PackedValue::transcode_consensus_with_descriptor(
            PACKED_VALUE_CODEC_VERSION,
            VALUE_SHAPE_CODEC_VERSION,
            consensus,
        )
        .map_err(codec_error)?;
        let mut record = Vec::with_capacity(2 + packed.as_bytes().len());
        record.extend_from_slice(&[RECORD_VERSION, KIND_CANONICAL_PACKED]);
        record.extend_from_slice(packed.as_bytes());
        Ok(EncodedRecord {
            record,
            shape: Some(shape),
        })
    } else {
        encode_prepared(typed.value(), typed.consensus_byte_len())
    }
}

/// Encode typed payload and reconstruction metadata from the same admitted value.
fn encode_prepared(
    value: &Value,
    consensus_byte_len: u32,
) -> Result<EncodedRecord, VmExecutionError> {
    let record = PackedValue::encode_with_prefix(
        PACKED_VALUE_CODEC_VERSION,
        value,
        &[RECORD_VERSION, KIND_CANONICAL_PACKED],
        consensus_byte_len,
    )
    .map_err(codec_error)?;
    let shape =
        ValueDescriptor::from_value(VALUE_SHAPE_CODEC_VERSION, value).map_err(codec_error)?;
    Ok(EncodedRecord {
        record,
        shape: Some(shape),
    })
}

/// Store one generic canonical value in an intrinsically reversible kind.
fn put_generic(
    conn: &Connection,
    value_hash: &MARFValue,
    canonical: &str,
    shapes: &mut ShapeCache,
) -> Result<(), VmExecutionError> {
    let encoded = encode_reversible(canonical);
    insert_or_reconcile(conn, value_hash, canonical, &encoded, shapes)
}

/// Classify one legacy canonical value for the offline migration.
pub fn encode_migrated(canonical: &str) -> Result<EncodedRecord, VmExecutionError> {
    let Some(consensus) = exact_lower_hex(canonical) else {
        return Ok(EncodedRecord {
            record: record(KIND_CANONICAL_UTF8, canonical.as_bytes()),
            shape: None,
        });
    };
    match PackedValue::transcode_consensus_with_descriptor(
        PACKED_VALUE_CODEC_VERSION,
        VALUE_SHAPE_CODEC_VERSION,
        &consensus,
    ) {
        Ok((packed, shape)) => Ok(EncodedRecord {
            record: record(KIND_CANONICAL_PACKED, packed.as_bytes()),
            shape: Some(shape),
        }),
        Err(_) => Ok(EncodedRecord {
            record: record(KIND_CANONICAL_HEX_BYTES, &consensus),
            shape: None,
        }),
    }
}

/// Fetch and decode one typed Binary V1 row with a one-column fast path.
pub fn get_typed(
    conn: &Connection,
    value_hash: &MARFValue,
    expected: &TypeSignature,
    epoch: &StacksEpochId,
) -> Result<Option<TypedValueResult>, VmExecutionError> {
    let packed_decode_error = {
        let mut statement = conn.prepare_cached(GET_TYPED).map_err(sql_error)?;
        let mut rows = statement
            .query(params![value_hash.as_bytes()])
            .map_err(sql_error)?;
        let Some(row) = rows.next().map_err(sql_error)? else {
            return Ok(None);
        };
        let record = row
            .get_ref(0)
            .map_err(sql_error)?
            .as_blob()
            .map_err(codec_error)?;
        let (kind, payload) = parse_record(record)?;
        match kind {
            KIND_CANONICAL_HEX_BYTES => {
                return Ok(Some(TypedValueResult {
                    value: decode_consensus_payload(payload, expected, epoch)?,
                    serialized_byte_len: payload.len() as u64,
                }));
            }
            KIND_CANONICAL_PACKED => {
                let packed = parse_packed_value(payload)?;
                match packed.decode(expected) {
                    Ok(decoded) => {
                        return Ok(Some(TypedValueResult {
                            value: decoded.value,
                            serialized_byte_len: u64::from(decoded.consensus_byte_len),
                        }));
                    }
                    Err(PackedValueError::Invariant(error)) => {
                        return Err(codec_invariant_error(error));
                    }
                    Err(error) => error,
                }
            }
            KIND_CANONICAL_UTF8 => {
                return Err(storage_error(
                    "typed Clarity read addressed a canonical UTF-8 Binary V1 row",
                ));
            }
            _ => unreachable!("parse_record rejects unknown kinds"),
        }
    };

    // Migrated records can contain historical, unsanitized tuple/list data that a narrower read
    // schema intentionally projects away. Preserve the one-column, zero-copy fast path above;
    // only resolve and audit reconstruction metadata when schema-directed decoding fails.
    // Packed error categories identify the detection site, not recoverability: valid historical
    // rows can produce record errors under a narrower schema. Only an internal codec invariant is
    // unambiguously ineligible for compatibility processing.
    let (record, shape) = load_with_shape(conn, value_hash)?
        .ok_or_else(|| storage_error("Binary V1 row disappeared during typed read"))?;
    let (kind, payload) = parse_record(&record)?;
    if kind != KIND_CANONICAL_PACKED {
        return Err(storage_error(
            "Binary V1 row changed during typed compatibility decode",
        ));
    }
    let shape = shape.ok_or_else(|| storage_error("packed row is missing its shape"))?;
    let packed = parse_packed_value(payload)?;
    let shape = parse_value_shape(&shape)?;
    let consensus = packed
        .audit_reconstruction_with_descriptor(shape)
        .map_err(codec_error)?;
    verify_canonical_hash(value_hash, &to_hex(&consensus))?;
    let value = decode_consensus_payload(&consensus, expected, epoch)
        .map_err(|error| compatibility_decode_error(packed_decode_error, error))?;
    Ok(Some(TypedValueResult {
        value,
        serialized_byte_len: consensus.len() as u64,
    }))
}

/// Fetch a packed value with lazy materialization, retaining compatibility decoding for historical rows.
pub fn get_stored(
    conn: &Connection,
    value_hash: &MARFValue,
    expected: &TypeSignature,
    epoch: &StacksEpochId,
) -> Result<Option<StoredValueResult>, VmExecutionError> {
    {
        let mut statement = conn.prepare_cached(GET_TYPED).map_err(sql_error)?;
        let mut rows = statement
            .query(params![value_hash.as_bytes()])
            .map_err(sql_error)?;
        let Some(row) = rows.next().map_err(sql_error)? else {
            return Ok(None);
        };
        let record = row
            .get_ref(0)
            .map_err(sql_error)?
            .as_blob()
            .map_err(|_| storage_error("Binary V1 value is not a blob"))?;
        let (kind, payload) = parse_record(record)?;
        if kind == KIND_CANONICAL_PACKED {
            match SharedPackedValue::copy_from(payload, expected, epoch) {
                Ok(value) => {
                    return Ok(Some(StoredValueResult {
                        serialized_byte_len: u64::from(value.consensus_byte_len()),
                        value: StoredValue::Packed(value),
                    }));
                }
                Err(PackedValueError::Invariant(error)) => {
                    return Err(codec_invariant_error(error));
                }
                Err(_) => {}
            }
        }
    }
    get_typed(conn, value_hash, expected, epoch).map(|value| {
        value.map(|value| StoredValueResult {
            value: StoredValue::Owned(value.value),
            serialized_byte_len: value.serialized_byte_len,
        })
    })
}

/// Fetch a generic row, reconstruct its exact canonical text, and verify its hash.
pub fn get_generic(
    conn: &Connection,
    value_hash: &MARFValue,
) -> Result<Option<String>, VmExecutionError> {
    let Some((record, shape)) = load_with_shape(conn, value_hash)? else {
        return Ok(None);
    };
    canonical_from_stored_record(value_hash, &record, shape.as_deref()).map(Some)
}

/// Reconstruct one stored record and verify its content-addressed key.
fn canonical_from_stored_record(
    value_hash: &MARFValue,
    record: &[u8],
    shape: Option<&[u8]>,
) -> Result<String, VmExecutionError> {
    canonical_from_stored_record_with_mode(value_hash, record, shape, false)
}

/// Exhaustively audit one record's canonical encoding and content-addressed key.
pub fn audit_stored_record(
    value_hash: &MARFValue,
    record: &[u8],
    shape: Option<&[u8]>,
) -> Result<String, VmExecutionError> {
    canonical_from_stored_record_with_mode(value_hash, record, shape, true)
}

/// Reconstruct a canonical value, optionally audit its physical encoding, and verify its hash.
fn canonical_from_stored_record_with_mode(
    value_hash: &MARFValue,
    record: &[u8],
    shape: Option<&[u8]>,
    audit: bool,
) -> Result<String, VmExecutionError> {
    let canonical = canonical_from_record(record, shape, audit)?;
    verify_canonical_hash(value_hash, &canonical)?;
    Ok(canonical)
}

/// Verify that canonical text reproduces its content-addressed key.
fn verify_canonical_hash(value_hash: &MARFValue, canonical: &str) -> Result<(), VmExecutionError> {
    if MARFValue::from_value(canonical) != *value_hash {
        return Err(storage_error(
            "Binary V1 record does not reproduce its data-table key",
        ));
    }
    Ok(())
}

/// Insert one Binary V1 metadata row with a binary block identifier.
pub fn insert_metadata(
    conn: &Connection,
    block_id: &[u8; 32],
    contract_id: &str,
    key: &str,
    value: &str,
) -> Result<(), VmExecutionError> {
    metadata::insert(conn, block_id, contract_id, key, value)
}

/// Fetch one Binary V1 metadata value by binary block identifier.
pub fn get_metadata(
    conn: &Connection,
    block_id: &[u8; 32],
    contract_id: &str,
    key: &str,
) -> Result<Option<String>, VmExecutionError> {
    metadata::get(conn, block_id, contract_id, key)
}

/// Rename Binary V1 metadata rows from one binary block identifier to another.
pub fn commit_metadata_to(
    conn: &Connection,
    from: &[u8; 32],
    to: &[u8; 32],
) -> Result<(), VmExecutionError> {
    metadata::commit(conn, from, to)
}

/// Delete Binary V1 metadata rows for a binary block identifier.
pub fn drop_metadata(conn: &Connection, block_id: &[u8; 32]) -> Result<(), VmExecutionError> {
    metadata::drop(conn, block_id)
}

/// Insert one metadata row while preserving its source storage class.
pub fn insert_metadata_row(conn: &Connection, row: &MetadataRow<'_>) -> rusqlite::Result<()> {
    metadata::insert_row(conn, row)
}

/// Visit metadata rows in legacy or Binary V1 physical representation.
pub fn visit_metadata_rows<E, F>(conn: &Connection, visit: F) -> Result<(), E>
where
    E: From<rusqlite::Error>,
    F: FnMut(&MetadataRow<'_>) -> Result<(), E>,
{
    metadata::visit_rows(conn, visit)
}

/// Visit metadata keys in deterministic ascending order.
pub fn visit_metadata_keys<E, F>(conn: &Connection, visit: F) -> Result<(), E>
where
    E: From<rusqlite::Error>,
    F: FnMut(&str) -> Result<(), E>,
{
    metadata::visit_keys(conn, visit)
}

/// Split a logical metadata key into its contract and metadata components.
pub fn parse_metadata_key(key: &str) -> Option<(&str, &str)> {
    metadata::parse_key(key)
}

/// Insert metadata through a Binary V1 backing store.
pub fn insert_store_metadata(
    store: &mut dyn ClarityBackingStore,
    contract: &QualifiedContractIdentifier,
    key: &str,
    value: &str,
) -> Result<(), VmExecutionError> {
    let block_id = store.get_open_chain_tip();
    insert_metadata(
        store.get_side_store(),
        block_id.as_bytes(),
        &contract.to_string(),
        key,
        value,
    )
}

/// Fetch metadata through a Binary V1 backing store.
pub fn get_store_metadata(
    store: &mut dyn ClarityBackingStore,
    contract: &QualifiedContractIdentifier,
    key: &str,
) -> Result<Option<String>, VmExecutionError> {
    let (block_id, _) = store.get_contract_hash(contract)?;
    get_metadata(
        store.get_side_store(),
        block_id.as_bytes(),
        &contract.to_string(),
        key,
    )
}

/// Fetch metadata at a block height through a Binary V1 backing store.
pub fn get_store_metadata_manual(
    store: &mut dyn ClarityBackingStore,
    at_height: u32,
    contract: &QualifiedContractIdentifier,
    key: &str,
) -> Result<Option<String>, VmExecutionError> {
    let block_id = store
        .get_block_at_height(at_height)
        .ok_or_else(|| RuntimeError::BadBlockHeight(at_height.to_string()))?;
    get_metadata(
        store.get_side_store(),
        block_id.as_bytes(),
        &contract.to_string(),
        key,
    )
}

/// Insert one content-addressed record or prove an existing representation is equivalent.
fn insert_or_reconcile(
    conn: &Connection,
    value_hash: &MARFValue,
    canonical: &str,
    encoded: &EncodedRecord,
    shapes: &mut ShapeCache,
) -> Result<(), VmExecutionError> {
    let shape_id = encoded
        .shape
        .as_ref()
        .map(|shape| shapes.intern(conn, shape))
        .transpose()?;
    let inserted = conn
        .prepare_cached(INSERT_DATA)
        .and_then(|mut statement| {
            statement.execute(params![value_hash.as_bytes(), encoded.record, shape_id])
        })
        .map_err(sql_error)?;
    if inserted == 1 {
        return Ok(());
    }

    let (existing_record, existing_shape) = load_with_shape(conn, value_hash)?
        .ok_or_else(|| storage_error("Binary V1 row disappeared during reconciliation"))?;
    if existing_record == encoded.record
        && existing_shape.as_deref() == encoded.shape.as_ref().map(ValueDescriptor::as_bytes)
    {
        return Ok(());
    }
    let existing_canonical =
        canonical_from_record(&existing_record, existing_shape.as_deref(), false)?;
    if existing_canonical == canonical {
        let (existing_kind, _) = parse_record(&existing_record)?;
        let (new_kind, _) = parse_record(&encoded.record)?;
        if existing_kind != KIND_CANONICAL_PACKED || new_kind != KIND_CANONICAL_PACKED {
            return Ok(());
        }
    }
    Err(storage_error(
        "conflicting Binary V1 payload for an existing value hash",
    ))
}

/// Intern a codec-validated descriptor, returning its database-local unsigned 32-bit ID.
fn intern_shape(conn: &Connection, descriptor: &[u8]) -> Result<i64, VmExecutionError> {
    conn.prepare_cached(INSERT_SHAPE)
        .and_then(|mut statement| statement.execute([descriptor]))
        .map_err(sql_error)?;
    let id: i64 = conn
        .prepare_cached(GET_SHAPE_ID)
        .and_then(|mut statement| statement.query_row([descriptor], |row| row.get(0)))
        .map_err(sql_error)?;
    if !(1..=i64::from(u32::MAX)).contains(&id) {
        return Err(storage_error("Binary V1 shape ID exceeds u32"));
    }
    Ok(id)
}

/// Load a record and resolve its optional normalized descriptor in one query.
fn load_with_shape(
    conn: &Connection,
    value_hash: &MARFValue,
) -> Result<Option<(Vec<u8>, Option<Vec<u8>>)>, VmExecutionError> {
    conn.prepare_cached(GET_GENERIC)
        .and_then(|mut statement| {
            statement
                .query_row(params![value_hash.as_bytes()], |row| {
                    Ok((row.get(0)?, row.get(1)?))
                })
                .optional()
        })
        .map_err(sql_error)
}

/// Encode generic canonical text without requiring schema metadata for reconstruction.
fn encode_reversible(canonical: &str) -> EncodedRecord {
    match exact_lower_hex(canonical) {
        Some(bytes) => EncodedRecord {
            record: record(KIND_CANONICAL_HEX_BYTES, &bytes),
            shape: None,
        },
        None => EncodedRecord {
            record: record(KIND_CANONICAL_UTF8, canonical.as_bytes()),
            shape: None,
        },
    }
}

/// Wrap a payload in the current record-version and kind envelope.
fn record(kind: u8, payload: &[u8]) -> Vec<u8> {
    debug_assert_matches!(
        kind,
        KIND_CANONICAL_UTF8 | KIND_CANONICAL_HEX_BYTES | KIND_CANONICAL_PACKED
    );
    let mut record = Vec::with_capacity(payload.len() + 2);
    record.extend_from_slice(&[RECORD_VERSION, kind]);
    record.extend_from_slice(payload);
    record
}

/// Validate a record envelope and borrow its kind and payload.
fn parse_record(record: &[u8]) -> Result<(u8, &[u8]), VmExecutionError> {
    let (&version, rest) = record
        .split_first()
        .ok_or_else(|| storage_error("empty Binary V1 record"))?;
    if version != RECORD_VERSION {
        return Err(storage_error(format!(
            "unsupported Binary V1 record version {version}"
        )));
    }
    let (&kind, payload) = rest
        .split_first()
        .ok_or_else(|| storage_error("Binary V1 record has no kind"))?;
    if !matches!(
        kind,
        KIND_CANONICAL_UTF8 | KIND_CANONICAL_HEX_BYTES | KIND_CANONICAL_PACKED
    ) {
        return Err(storage_error(format!(
            "unknown Binary V1 payload kind {kind}"
        )));
    }
    Ok((kind, payload))
}

/// Reconstruct canonical text from any record kind, with optional packed canonicality auditing.
fn canonical_from_record(
    record: &[u8],
    shape: Option<&[u8]>,
    audit: bool,
) -> Result<String, VmExecutionError> {
    let (kind, payload) = parse_record(record)?;
    match kind {
        KIND_CANONICAL_UTF8 => {
            if shape.is_some() {
                return Err(storage_error("canonical UTF-8 row references a shape"));
            }
            str::from_utf8(payload)
                .map(str::to_owned)
                .map_err(codec_error)
        }
        KIND_CANONICAL_HEX_BYTES => {
            if shape.is_some() {
                return Err(storage_error("canonical hex row references a shape"));
            }
            Ok(to_hex(payload))
        }
        KIND_CANONICAL_PACKED => {
            let shape = shape.ok_or_else(|| storage_error("packed row is missing its shape"))?;
            let packed = parse_packed_value(payload)?;
            let shape = parse_value_shape(shape)?;
            let consensus = if audit {
                packed.audit_reconstruction_with_descriptor(shape)
            } else {
                packed.reconstruct_consensus_with_descriptor(shape)
            };
            consensus
                .map(|consensus| to_hex(&consensus))
                .map_err(codec_error)
        }
        _ => unreachable!("parse_record rejects unknown kinds"),
    }
}

/// Parse a packed payload and enforce the version admitted by Binary V1 storage.
fn parse_packed_value(payload: &[u8]) -> Result<PackedValueRef<'_>, VmExecutionError> {
    let packed = PackedValueRef::parse(payload).map_err(codec_error)?;
    if packed.version() != PACKED_VALUE_CODEC_VERSION {
        return Err(storage_error(format!(
            "Binary V1 storage does not admit packed value version {}",
            packed.version().as_u8()
        )));
    }
    Ok(packed)
}

/// Parse a shape envelope and enforce the version admitted by Binary V1 storage.
fn parse_value_shape(descriptor: &[u8]) -> Result<ValueDescriptorRef<'_>, VmExecutionError> {
    let shape = ValueDescriptorRef::parse(descriptor).map_err(codec_error)?;
    if shape.version() != VALUE_SHAPE_CODEC_VERSION {
        return Err(storage_error(format!(
            "Binary V1 storage does not admit value-shape version {}",
            shape.version().as_u8()
        )));
    }
    Ok(shape)
}

/// Decode even-length lowercase hexadecimal text, rejecting every other representation.
fn exact_lower_hex(canonical: &str) -> Option<Vec<u8>> {
    if !canonical.len().is_multiple_of(2)
        || !canonical
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return None;
    }
    hex_bytes(canonical).ok()
}

/// Decode raw consensus bytes, restoring callable metadata supplied only by the read schema.
fn decode_consensus_payload(
    payload: &[u8],
    expected: &TypeSignature,
    epoch: &StacksEpochId,
) -> Result<Value, VmExecutionError> {
    match Value::try_deserialize_bytes_exact_at_epoch(payload, expected, epoch) {
        Ok(value) => Ok(value),
        Err(error)
            if matches!(
                expected,
                TypeSignature::CallableType(_) | TypeSignature::TraitReferenceType(_)
            ) =>
        {
            let principal = Value::try_deserialize_bytes_exact_at_epoch(
                payload,
                &TypeSignature::PrincipalType,
                epoch,
            )
            .map_err(codec_error)?;
            let Value::Principal(PrincipalData::Contract(contract_identifier)) = principal else {
                return Err(codec_error(error));
            };
            let trait_identifier = match expected {
                TypeSignature::CallableType(
                    clarity::vm::types::signatures::CallableSubtype::Principal(expected_contract),
                ) => {
                    if contract_identifier != *expected_contract {
                        return Err(codec_error(error));
                    }
                    None
                }
                TypeSignature::CallableType(
                    clarity::vm::types::signatures::CallableSubtype::Trait(trait_identifier),
                )
                | TypeSignature::TraitReferenceType(trait_identifier) => {
                    Some(Box::new(trait_identifier.clone()))
                }
                _ => unreachable!("guard restricts callable schema variants"),
            };
            Ok(Value::CallableContract(clarity::vm::types::CallableData {
                contract_identifier,
                trait_identifier,
            }))
        }
        Err(error) => Err(codec_error(error)),
    }
}

/// Convert a codec or UTF-8 failure into the backing store's database error type.
fn codec_error(error: impl std::fmt::Display) -> VmExecutionError {
    storage_error(error.to_string())
}

/// Convert an internal codec invariant into an internal VM expectation failure.
fn codec_invariant_error(error: PackedCodecInvariant) -> VmExecutionError {
    VmInternalError::Expect(format!("packed codec invariant failed: {error}")).into()
}

/// Preserve both failed decoding strategies when neither can interpret a typed value.
fn compatibility_decode_error(
    packed_error: PackedValueError,
    consensus_error: VmExecutionError,
) -> VmExecutionError {
    storage_error(format!(
        "packed typed decode failed: {packed_error}; compatibility consensus decode failed: \
         {consensus_error}"
    ))
}

/// Convert a SQLite failure into the backing store's database error type.
fn sql_error(error: rusqlite::Error) -> VmExecutionError {
    storage_error(error.to_string())
}

/// Construct a backing-store database error from a validated storage invariant failure.
fn storage_error(message: impl Into<String>) -> VmExecutionError {
    VmInternalError::DBError(message.into()).into()
}

#[cfg(test)]
mod tests {
    use clarity::vm::types::TypeSignature;
    use clarity::vm::types::codec::packed::PackedRecordError;
    use stacks_common::types::StacksEpochId;
    use stacks_common::util::hash::to_hex;

    use super::*;

    const EPOCH: StacksEpochId = StacksEpochId::Epoch40;

    /// Retain an admitted packed owner for write-admission fixtures.
    fn shared_fixture(value: &Value) -> SharedPackedValue {
        let packed = PackedValue::encode(PACKED_VALUE_CODEC_VERSION, value).unwrap();
        SharedPackedValue::copy_from(
            packed.as_bytes(),
            &TypeSignature::type_of(value).unwrap(),
            &EPOCH,
        )
        .unwrap()
    }

    /// Admission remains conservative for lane compression and retained projections/composites.
    #[test]
    fn inline_admission_bounds_shared_values_without_materialization() {
        let large = shared_fixture(&Value::buff_from(vec![7; 4096]).unwrap());
        let list = shared_fixture(
            &Value::list_from(vec![Value::buff_from(vec![7; 4096]).unwrap(); 3]).unwrap(),
        );
        let small = shared_fixture(&Value::UInt(1));
        let mut cases = vec![
            (large.clone(), true),
            (large.clone().sliced_sequence(0, 8).unwrap().unwrap(), false),
            (
                large
                    .clone()
                    .concat_sequence(large.clone(), &EPOCH)
                    .unwrap(),
                true,
            ),
            (list.clone().filtered_list(vec![0, 2]).unwrap(), true),
            (list.list_child(1).unwrap().unwrap(), true),
            (
                SharedPackedValue::tuple(vec![("value".try_into().unwrap(), large)], &EPOCH)
                    .unwrap(),
                true,
            ),
            (
                SharedPackedValue::list(vec![small; 100], &EPOCH).unwrap(),
                true,
            ),
            (
                shared_fixture(&Value::list_from(vec![Value::Bool(true); 64]).unwrap()),
                false,
            ),
            (
                shared_fixture(&Value::list_from(vec![Value::UInt(0); 64]).unwrap()),
                true,
            ),
        ];
        for size in [0, 1, 23, 24, 25, 30, 31] {
            cases.push((
                shared_fixture(&Value::buff_from(vec![0; size]).unwrap()),
                size > 24,
            ));
        }
        for (shared, reject) in cases {
            let data = DataStoreValue::Typed(TypedValueData::prepare_shared(shared).unwrap());
            let before = SharedPackedValue::materialization_count();
            let candidate = encode_inline_candidate(&data, 30).unwrap();
            assert_eq!(SharedPackedValue::materialization_count(), before);
            assert_eq!(candidate.is_none(), reject);
            let full = encode_entry(&data).unwrap();
            if let Some(candidate) = candidate {
                assert_eq!(candidate.record, full.record);
                assert_eq!(
                    candidate.shape.as_ref().map(|s| s.as_bytes()),
                    full.shape.as_ref().map(|s| s.as_bytes())
                );
            } else {
                assert!(full.record.len() > 30);
            }
        }
    }

    /// Canonical text rejection must not reject a large consensus string that packs small.
    #[test]
    fn inline_admission_preserves_small_packed_canonical_values() {
        for text in ["g".repeat(1000), "a".repeat(1001), "abcdefg".repeat(100)] {
            assert!(
                encode_inline_candidate(&DataStoreValue::Canonical(text), 30)
                    .unwrap()
                    .is_none()
            );
        }
        for count in [0, 1, 64, 128] {
            let value = Value::list_from(vec![Value::Bool(true); count]).unwrap();
            let data = DataStoreValue::Canonical(to_hex(&value.serialize_to_vec().unwrap()));
            let full = encode_entry(&data).unwrap();
            let candidate = encode_inline_candidate(&data, 30)
                .unwrap()
                .expect("compressible canonical value");
            assert_eq!(full.record, candidate.record);
            assert!(candidate.record.len() <= 30);
        }
    }

    fn legacy_schema(conn: &Connection) {
        conn.execute(
            "CREATE TABLE data_table (key TEXT PRIMARY KEY, value TEXT)",
            [],
        )
        .unwrap();
        conn.execute(
            "CREATE TABLE metadata_table (
                key TEXT NOT NULL, blockhash TEXT, value TEXT,
                UNIQUE(key, blockhash))",
            [],
        )
        .unwrap();
        conn.execute(
            "CREATE INDEX md_blockhashes ON metadata_table(blockhash)",
            [],
        )
        .unwrap();
    }

    fn typed(value: Value) -> (String, TypedValueData, MARFValue) {
        let data = TypedValueData::prepare(value).unwrap();
        let canonical = data.canonical().to_owned();
        let hash = MARFValue::from_value(&canonical);
        (canonical, data, hash)
    }

    fn store_typed(conn: &Connection, hash: &MARFValue, data: TypedValueData) {
        put_typed(conn, hash, data, &mut ShapeCache::default()).unwrap();
    }

    fn store_generic(conn: &Connection, hash: &MARFValue, canonical: &str) {
        put_generic(conn, hash, canonical, &mut ShapeCache::default()).unwrap();
    }

    fn reconcile(conn: &Connection, hash: &MARFValue, canonical: &str, encoded: &EncodedRecord) {
        insert_or_reconcile(conn, hash, canonical, encoded, &mut ShapeCache::default()).unwrap();
    }

    fn initialize_completed(conn: &Connection) {
        initialize_migration_destination(conn).unwrap();
        create_migration_indexes(conn).unwrap();
        finalize_migration_destination(conn).unwrap();
    }

    #[test]
    fn detects_legacy_and_initializes_only_empty_tables() {
        let conn = Connection::open_in_memory().unwrap();
        legacy_schema(&conn);
        assert_eq!(detect(&conn).unwrap(), ValueStorageFormat::LegacyText);
        initialize_empty(&conn).unwrap();
        assert_eq!(detect(&conn).unwrap(), ValueStorageFormat::BinaryV1);

        let populated = Connection::open_in_memory().unwrap();
        legacy_schema(&populated);
        populated
            .execute("INSERT INTO data_table VALUES ('key', 'value')", [])
            .unwrap();
        assert!(initialize_empty(&populated).is_err());
    }

    #[test]
    fn incomplete_and_unknown_destinations_fail_closed() {
        let conn = Connection::open_in_memory().unwrap();
        initialize_migration_destination(&conn).unwrap();
        assert!(detect(&conn).is_err());
        assert!(finalize_migration_destination(&conn).is_err());
        create_migration_indexes(&conn).unwrap();
        finalize_migration_destination(&conn).unwrap();
        assert_eq!(detect(&conn).unwrap(), ValueStorageFormat::BinaryV1);
        conn.execute(
            "UPDATE clarity_side_store_format SET storage_version = 99",
            [],
        )
        .unwrap();
        assert!(detect(&conn).is_err());
    }

    #[test]
    fn physical_schema_drift_fails_closed() {
        let conn = Connection::open_in_memory().unwrap();
        initialize_completed(&conn);
        conn.execute_batch(
            "DROP TABLE clarity_side_store_format;
             CREATE TABLE clarity_side_store_format (
                 singleton INTEGER PRIMARY KEY,
                 storage_version INTEGER NOT NULL,
                 record_version INTEGER NOT NULL,
                 shape_version INTEGER NOT NULL,
                 complete INTEGER NOT NULL
             );",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO clarity_side_store_format VALUES (1, ?1, ?2, ?3, 1)",
            params![
                schema::STORAGE_VERSION,
                i64::from(RECORD_VERSION),
                schema::SHAPE_VERSION
            ],
        )
        .unwrap();

        let error = detect(&conn).unwrap_err().to_string();
        assert!(error.contains("clarity_side_store_format physical schema"));
    }

    #[test]
    fn typed_and_generic_rows_round_trip_with_normalized_shapes() {
        let conn = Connection::open_in_memory().unwrap();
        initialize_completed(&conn);

        let value = Value::UInt(42);
        let (canonical, data, hash) = typed(value.clone());
        let expected_packed =
            PackedValue::encode(PACKED_VALUE_CODEC_VERSION, data.value()).unwrap();
        let expected_record = record(KIND_CANONICAL_PACKED, expected_packed.as_bytes());
        store_typed(&conn, &hash, data);
        let stored_record: Vec<u8> = conn
            .query_row(
                "SELECT value FROM data_table WHERE key = ?1",
                [hash.as_bytes()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stored_record, expected_record);
        assert_eq!(
            get_typed(&conn, &hash, &TypeSignature::UIntType, &EPOCH)
                .unwrap()
                .unwrap()
                .value,
            value
        );
        assert_eq!(
            get_generic(&conn, &hash).unwrap().as_deref(),
            Some(canonical.as_str())
        );
        let shape_count: u64 = conn
            .query_row("SELECT COUNT(*) FROM clarity_value_shapes", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(shape_count, 1);

        let generic = "not hexadecimal";
        let generic_hash = MARFValue::from_value(generic);
        store_generic(&conn, &generic_hash, generic);
        assert_eq!(
            get_generic(&conn, &generic_hash).unwrap().as_deref(),
            Some(generic)
        );
    }

    #[test]
    fn live_batch_reuses_duplicate_values_and_shape_ids() {
        let conn = Connection::open_in_memory().unwrap();
        initialize_completed(&conn);

        let first = TypedValueData::prepare(Value::UInt(1)).unwrap();
        let second = TypedValueData::prepare(Value::UInt(2)).unwrap();
        let repeated = TypedValueData::prepare(Value::UInt(1)).unwrap();
        let (keys, hashes) = put_entries(
            &conn,
            vec![
                DataStoreEntry {
                    key: "first".into(),
                    value: DataStoreValue::Typed(first),
                },
                DataStoreEntry {
                    key: "second".into(),
                    value: DataStoreValue::Typed(second),
                },
                DataStoreEntry {
                    key: "repeated".into(),
                    value: DataStoreValue::Typed(repeated),
                },
            ],
        )
        .unwrap();

        assert_eq!(keys, ["first", "second", "repeated"]);
        assert_eq!(hashes[0], hashes[2]);
        let (rows, shapes): (u64, u64) = conn
            .query_row(
                "SELECT (SELECT COUNT(*) FROM data_table),
                        (SELECT COUNT(*) FROM clarity_value_shapes)",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!((rows, shapes), (2, 1));
    }

    #[test]
    fn live_batch_reuses_equal_values_across_generic_and_typed_writes() {
        let conn = Connection::open_in_memory().unwrap();
        initialize_completed(&conn);

        let typed = TypedValueData::prepare(Value::Bool(true)).unwrap();
        let canonical = typed.canonical().to_owned();
        let (_, hashes) = put_entries(
            &conn,
            vec![
                DataStoreEntry {
                    key: "generic".into(),
                    value: DataStoreValue::Canonical(canonical),
                },
                DataStoreEntry {
                    key: "typed".into(),
                    value: DataStoreValue::Typed(typed),
                },
            ],
        )
        .unwrap();

        assert_eq!(hashes[0], hashes[1]);
        assert_eq!(
            get_typed(&conn, &hashes[0], &TypeSignature::BoolType, &EPOCH)
                .unwrap()
                .unwrap()
                .value,
            Value::Bool(true)
        );
    }

    #[test]
    fn empty_generic_payload_is_the_minimum_valid_record() {
        let conn = Connection::open_in_memory().unwrap();
        initialize_completed(&conn);

        let canonical = "";
        let hash = MARFValue::from_value(canonical);
        store_generic(&conn, &hash, canonical);

        let (storage_class, record_len, shape_id): (String, u64, Option<i64>) = conn
            .query_row(
                "SELECT typeof(value), length(value), value_shape_id
                 FROM data_table WHERE key = ?1",
                [hash.as_bytes()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(storage_class, "blob");
        assert_eq!(record_len, 2);
        assert_eq!(shape_id, None);
        assert_eq!(get_generic(&conn, &hash).unwrap().as_deref(), Some(""));
    }

    #[test]
    fn migrated_values_pack_and_reconstruct_without_a_schema() {
        let conn = Connection::open_in_memory().unwrap();
        initialize_completed(&conn);

        let value = Value::Tuple(
            clarity::vm::types::TupleData::from_data(vec![(
                clarity::vm::ClarityName::from_literal("answer"),
                Value::UInt(42),
            )])
            .unwrap(),
        );
        let canonical = to_hex(&value.serialize_to_vec().unwrap());
        let hash = MARFValue::from_value(&canonical);
        let encoded = encode_migrated(&canonical).unwrap();
        assert!(encoded.shape().is_some());
        reconcile(&conn, &hash, &canonical, &encoded);
        assert_eq!(
            get_generic(&conn, &hash).unwrap().as_deref(),
            Some(canonical.as_str())
        );

        let non_value_hex = "ff00";
        let hash = MARFValue::from_value(non_value_hex);
        let encoded = encode_migrated(non_value_hex).unwrap();
        assert!(encoded.shape().is_none());
        reconcile(&conn, &hash, non_value_hex, &encoded);
        assert_eq!(
            get_generic(&conn, &hash).unwrap().as_deref(),
            Some(non_value_hex)
        );
    }

    #[test]
    fn metadata_uses_binary_block_identifiers() {
        let conn = Connection::open_in_memory().unwrap();
        initialize_completed(&conn);
        let first = [0x11; 32];
        let second = [0x22; 32];

        insert_metadata(&conn, &first, "ST1.contract", "source", "code").unwrap();
        assert_eq!(
            get_metadata(&conn, &first, "ST1.contract", "source").unwrap(),
            Some("code".into())
        );
        assert_eq!(
            conn.query_row("SELECT typeof(blockhash) FROM metadata_table", [], |row| {
                row.get::<_, String>(0)
            },)
                .unwrap(),
            "blob"
        );

        commit_metadata_to(&conn, &first, &second).unwrap();
        assert_eq!(
            get_metadata(&conn, &second, "ST1.contract", "source").unwrap(),
            Some("code".into())
        );
        drop_metadata(&conn, &second).unwrap();
        assert_eq!(
            get_metadata(&conn, &second, "ST1.contract", "source").unwrap(),
            None
        );
    }

    #[test]
    fn one_hash_can_serve_typed_and_generic_capabilities() {
        let conn = Connection::open_in_memory().unwrap();
        initialize_completed(&conn);
        let value = Value::Bool(true);
        let (canonical, data, hash) = typed(value.clone());
        store_typed(&conn, &hash, data);
        store_generic(&conn, &hash, &canonical);
        assert_eq!(
            get_generic(&conn, &hash).unwrap().as_deref(),
            Some(canonical.as_str())
        );
        assert_eq!(
            get_typed(&conn, &hash, &TypeSignature::BoolType, &EPOCH)
                .unwrap()
                .unwrap()
                .value,
            value
        );
    }

    #[test]
    fn typed_read_does_not_access_the_shape_dictionary() {
        let conn = Connection::open_in_memory().unwrap();
        initialize_completed(&conn);
        let value = Value::UInt(42);
        let (canonical, data, hash) = typed(value.clone());
        store_typed(&conn, &hash, data);

        // A typed read needs only the packed record and its caller-supplied
        // schema. Removing the reconstruction-only dictionary makes an
        // accidental dictionary dependency fail this test at query time.
        conn.execute_batch("DROP TABLE clarity_value_shapes")
            .unwrap();

        assert_eq!(
            get_typed(&conn, &hash, &TypeSignature::UIntType, &EPOCH)
                .unwrap()
                .unwrap()
                .value,
            value
        );
    }

    #[test]
    fn migrated_historical_value_uses_audited_compatibility_decode() {
        let conn = Connection::open_in_memory().unwrap();
        initialize_completed(&conn);

        let historical = Value::Tuple(
            clarity::vm::types::TupleData::from_data(vec![
                (
                    clarity::vm::ClarityName::from_literal("active"),
                    Value::Bool(true),
                ),
                (
                    clarity::vm::ClarityName::from_literal("obsolete"),
                    Value::UInt(42),
                ),
            ])
            .unwrap(),
        );
        let consensus = historical.serialize_to_vec().unwrap();
        let canonical = to_hex(&consensus);
        let hash = MARFValue::from_value(&canonical);
        let expected = TypeSignature::TupleType(
            clarity::vm::types::TupleTypeSignature::try_from(vec![(
                clarity::vm::ClarityName::from_literal("active"),
                TypeSignature::BoolType,
            )])
            .unwrap(),
        );
        let sanitized =
            Value::try_deserialize_bytes_exact_at_epoch(&consensus, &expected, &EPOCH).unwrap();

        let encoded = encode_migrated(&canonical).unwrap();
        reconcile(&conn, &hash, &canonical, &encoded);

        let record: Vec<u8> = conn
            .query_row(
                "SELECT value FROM data_table WHERE key = ?1",
                [hash.as_bytes()],
                |row| row.get(0),
            )
            .unwrap();
        let (_, payload) = parse_record(&record).unwrap();
        let Err(fast_error) = parse_packed_value(payload).unwrap().decode(&expected) else {
            panic!("the narrower schema should require compatibility decoding");
        };
        assert!(matches!(
            fast_error,
            PackedValueError::Record(PackedRecordError::FixedTupleTrailingBytes { .. })
        ));

        let decoded = get_typed(&conn, &hash, &expected, &EPOCH).unwrap().unwrap();
        assert_eq!(decoded.value, sanitized);
        assert_eq!(decoded.serialized_byte_len, consensus.len() as u64);

        conn.execute(
            "UPDATE data_table SET value_shape_id = NULL WHERE key = ?1",
            [hash.as_bytes()],
        )
        .unwrap();
        let error = get_typed(&conn, &hash, &expected, &EPOCH)
            .unwrap_err()
            .to_string();
        assert!(error.contains("packed row is missing its shape"));
        assert!(!error.contains("packed typed decode failed"));
    }

    #[test]
    fn failed_compatibility_decode_preserves_both_errors() {
        let conn = Connection::open_in_memory().unwrap();
        initialize_completed(&conn);
        let (_, data, hash) = typed(Value::UInt(42));
        store_typed(&conn, &hash, data);

        let error = get_typed(&conn, &hash, &TypeSignature::BoolType, &EPOCH)
            .unwrap_err()
            .to_string();
        assert!(error.contains("packed typed decode failed:"));
        assert!(error.contains("compatibility consensus decode failed:"));
    }

    #[test]
    fn codec_invariants_are_internal_expectation_failures() {
        let error = codec_invariant_error(PackedCodecInvariant::FixedTupleClassificationChanged);
        let VmExecutionError::Internal(VmInternalError::Expect(message)) = error else {
            panic!("codec invariant should map to VmInternalError::Expect");
        };
        assert_eq!(
            message,
            "packed codec invariant failed: canonical fixed tuple classification changed"
        );
    }

    #[test]
    fn typed_consensus_fallback_rejects_trailing_bytes() {
        let conn = Connection::open_in_memory().unwrap();
        initialize_completed(&conn);

        let mut consensus = Value::UInt(42).serialize_to_vec().unwrap();
        consensus.push(0xff);
        let canonical = to_hex(&consensus);
        let hash = MARFValue::from_value(&canonical);
        let encoded = encode_migrated(&canonical).unwrap();
        assert!(encoded.shape().is_none());
        reconcile(&conn, &hash, &canonical, &encoded);

        assert!(get_typed(&conn, &hash, &TypeSignature::UIntType, &EPOCH).is_err());
    }

    #[test]
    fn generic_read_rejects_a_record_that_does_not_match_its_key() {
        let conn = Connection::open_in_memory().unwrap();
        initialize_completed(&conn);
        let canonical = to_hex(&Value::UInt(42).serialize_to_vec().unwrap());
        let hash = MARFValue::from_value(&canonical);
        let encoded = encode_migrated(&canonical).unwrap();
        reconcile(&conn, &hash, &canonical, &encoded);

        conn.execute(
            "UPDATE data_table SET value = ?1, value_shape_id = NULL WHERE key = ?2",
            params![record(KIND_CANONICAL_HEX_BYTES, &[0xff]), hash.as_bytes()],
        )
        .unwrap();
        assert!(get_generic(&conn, &hash).is_err());
    }

    #[test]
    fn row_and_interned_shape_rollback_atomically() {
        let mut conn = Connection::open_in_memory().unwrap();
        initialize_completed(&conn);
        let value = Value::UInt(42);
        let (canonical, data, hash) = typed(value.clone());

        let transaction = conn.transaction().unwrap();
        store_typed(&transaction, &hash, data);
        transaction.rollback().unwrap();

        let (rows, shapes): (u64, u64) = conn
            .query_row(
                "SELECT (SELECT COUNT(*) FROM data_table),
                        (SELECT COUNT(*) FROM clarity_value_shapes)",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!((rows, shapes), (0, 0));

        let (_, repeated_data, repeated_hash) = typed(value.clone());
        assert_eq!(repeated_hash, hash);
        store_typed(&conn, &hash, repeated_data);
        assert_eq!(
            get_typed(&conn, &hash, &TypeSignature::UIntType, &EPOCH)
                .unwrap()
                .unwrap()
                .value,
            value
        );
    }

    #[test]
    fn migration_writer_reuses_cached_shape_ids() {
        let conn = Connection::open_in_memory().unwrap();
        initialize_migration_destination(&conn).unwrap();
        let mut writer = MigrationWriter::new();

        for number in [1, 2] {
            let value = Value::Tuple(
                clarity::vm::types::TupleData::from_data(vec![(
                    clarity::vm::ClarityName::from_literal("value"),
                    Value::UInt(number),
                )])
                .unwrap(),
            );
            let canonical = to_hex(&value.serialize_to_vec().unwrap());
            let value_hash = MARFValue::from_value(&canonical);
            let encoded = encode_migrated(&canonical).unwrap();
            writer
                .put(&conn, &value_hash, &canonical, &encoded)
                .unwrap();
        }

        assert_eq!(writer.shape_cache_stats(), (1, 1, 0));
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM clarity_value_shapes", [], |row| {
                row.get::<_, u64>(0)
            })
            .unwrap(),
            1
        );
    }
}
