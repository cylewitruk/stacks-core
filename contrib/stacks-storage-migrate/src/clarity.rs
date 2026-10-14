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

//! Clarity-specific value extraction; generic trie conversion only invokes a leaf transform.

use std::path::Path;
use std::thread;
use std::time::Instant;

use blockstack_lib::clarity_vm::database::binary_value_store::{
    self, MetadataBlockId, MetadataRow, ValueStorageFormat,
};
use blockstack_lib::clarity_vm::database::stable_value_store::{
    PhysicalDescriptorBatch, PhysicalValue, StableValueStore,
};
use rusqlite::{Connection, params};
use stacks_common::util::hash::hex_bytes;

use crate::{Result, memberships, space, value_pipeline};

/// Clarity leaf policy backed by an immutable mapped commitment lookup.
pub struct ClarityValues {
    /// Shared read-only lookup; source extraction tables are never queried per leaf.
    pub lookup: crate::value_lookup::ValueLookup,
}

impl ClarityValues {
    /// Build final stable files directly from legacy text; scratch publication has no per-row sync.
    pub fn prepare(
        source: &Connection,
        destination: &Connection,
        directory: &Path,
        db_name: &str,
    ) -> Result<Self> {
        let source_format = binary_value_store::detect(source)?;
        if source_format == ValueStorageFormat::BinaryV1 {
            // Source-only adapter for retained legacy tries with already packed SQL values.
            binary_value_store::verify_shape_references(source)?;
        }
        binary_value_store::initialize_migration_destination(destination)?;
        Self::metadata(source, destination, source_format)?;
        StableValueStore::initialize_index(destination)?;
        memberships::initialize(destination)?;
        let generation_name = format!("{db_name}.stable-v1");
        let mut store =
            StableValueStore::create(&directory.join(&generation_name), rand::random())?;
        let mut space = space::Guard::new(directory, 4 * 1024 * 1024 * 1024 + 64 * 1024 * 1024)?;
        let sql = match source_format {
            ValueStorageFormat::LegacyText => {
                "SELECT key,value,NULL FROM data_table ORDER BY rowid"
            }
            ValueStorageFormat::BinaryV1 => {
                "SELECT d.key,d.value,s.descriptor FROM data_table d LEFT JOIN clarity_value_shapes s ON s.id=d.value_shape_id ORDER BY d.rowid"
            }
        };
        let mut query = source.prepare(sql)?;
        let mut rows = query.query([])?;
        let workers = thread::available_parallelism()
            .map_or(1, |count| count.get())
            .min(8);
        let mut count = 0u64;
        let mut last = Instant::now();
        loop {
            space.check()?;
            let mut input = Vec::new();
            let mut batch_bytes = 0usize;
            while input.len() < 4096 && batch_bytes < 16 * 1024 * 1024 {
                let Some(row) = rows.next()? else {
                    break;
                };
                let value = value_pipeline::Input::read(row, source_format)?;
                batch_bytes = batch_bytes
                    .checked_add(value.bytes())
                    .ok_or("source batch size overflow")?;
                input.push(value);
            }
            if input.is_empty() {
                break;
            }
            let prepared = value_pipeline::prepare(&input, workers)?;
            count += prepared.len() as u64;
            drop(input);
            let transaction = destination.unchecked_transaction()?;
            let mut external = Vec::new();
            let mut inline_insert = transaction
                .prepare_cached("INSERT INTO canonical_inline_values VALUES(?1,?2,?3)")?;
            for value in prepared {
                if value.inline {
                    inline_insert.execute(params![
                        value.commitment.0.as_slice(),
                        value.payload,
                        value.descriptor
                    ])?;
                } else {
                    external.push((value.commitment, value.payload, value.descriptor));
                }
            }
            drop(inline_insert);
            let records: Vec<_> = external
                .iter()
                .map(|(commitment, payload, descriptor)| PhysicalValue {
                    commitment,
                    payload,
                    descriptor,
                })
                .collect();
            let mut descriptors = PhysicalDescriptorBatch::new(&transaction, 16 * 1024 * 1024);
            let ids = store.append_physical_batch(&records, &mut descriptors)?;
            let mut insert = transaction
                .prepare_cached("INSERT INTO canonical_external_values VALUES(?1,?2)")?;
            for ((commitment, _, _), id) in external.iter().zip(ids) {
                insert.execute(params![commitment.0.as_slice(), id.get()])?;
            }
            drop(insert);
            drop(descriptors);
            transaction.commit()?;
            if last.elapsed().as_secs() >= 10 {
                eprintln!("canonical clarity_values={count}");
                last = Instant::now();
            }
        }
        eprintln!("canonical indexing_value_lookups source_values={count}");
        memberships::index(destination)?;
        // Complete private files become durable before any referencing format registration.
        store.sync_unpublished()?;
        destination.execute_batch("CREATE TABLE clarity_stable_format(singleton INTEGER PRIMARY KEY CHECK(singleton=1),version INTEGER NOT NULL CHECK(version=1),path TEXT NOT NULL,store_id BLOB NOT NULL CHECK(length(store_id)=16))")?;
        destination.execute(
            "INSERT INTO clarity_stable_format VALUES(1,1,?1,?2)",
            params![generation_name, store.store_id().as_slice()],
        )?;
        binary_value_store::create_migration_indexes(destination)?;
        Ok(Self {
            lookup: crate::value_lookup::ValueLookup::build(destination, directory)?,
        })
    }

    /// Preserve nullable metadata and normalize only the legacy block-hash representation.
    fn metadata(
        source: &Connection,
        destination: &Connection,
        format: ValueStorageFormat,
    ) -> Result<()> {
        let mut query =
            source.prepare("SELECT key,blockhash,value FROM metadata_table ORDER BY rowid")?;
        let mut rows = query.query([])?;
        destination.execute_batch("BEGIN")?;
        let mut count = 0;
        while let Some(row) = rows.next()? {
            let block = match format {
                ValueStorageFormat::LegacyText => row
                    .get::<_, Option<String>>(1)?
                    .map(|text| -> Result<[u8; 32]> {
                        hex_bytes(&text)?
                            .try_into()
                            .map_err(|_| "invalid metadata block hash".into())
                    })
                    .transpose()?,
                ValueStorageFormat::BinaryV1 => row
                    .get::<_, Option<Vec<u8>>>(1)?
                    .map(|bytes| -> Result<[u8; 32]> {
                        bytes
                            .try_into()
                            .map_err(|_| "invalid binary metadata block hash".into())
                    })
                    .transpose()?,
            };
            let value = row.get::<_, Option<String>>(2)?;
            binary_value_store::insert_metadata_row(
                destination,
                &MetadataRow {
                    key: row.get_ref(0)?.as_str()?,
                    block_id: block
                        .as_ref()
                        .map_or(MetadataBlockId::Null, |bytes| MetadataBlockId::Bytes(bytes)),
                    value: value.as_deref(),
                },
            )?;
            count += 1;
            if count % 4096 == 0 {
                destination.execute_batch("COMMIT; BEGIN")?;
            }
        }
        destination.execute_batch("COMMIT")?;
        Ok(())
    }

    /// Reconstruct metadata from source, then verify and map an explicitly retained extraction.
    pub fn reuse(
        source: &Connection,
        destination: &Connection,
        directory: &Path,
        db_name: &str,
    ) -> Result<Self> {
        destination.execute_batch("DELETE FROM metadata_table")?;
        Self::metadata(source, destination, binary_value_store::detect(source)?)?;
        let lookup = crate::value_lookup::ValueLookup::build(destination, directory)?;
        crate::reused_values::verify(source, &directory.join(db_name), &lookup)?;
        Ok(Self { lookup })
    }

    /// Remove extraction-only lookup rows and mark the verified Clarity schema complete.
    pub fn finish(self, db: &Connection) -> Result<()> {
        db.execute_batch("DROP TABLE canonical_inline_values")?;
        binary_value_store::finalize_migration_destination(db)?;
        Ok(())
    }
}
