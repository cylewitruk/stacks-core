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

//! Immutable, prefix-indexed commitment lookup for offline trie transformation.

use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::Path;

use blockstack_lib::chainstate::stacks::index::inline_value::InlineValue;
use blockstack_lib::chainstate::stacks::index::{Error as MarfError, MARFValue, TrieLeaf};
use blockstack_lib::clarity_vm::database::binary_value_store;
use memmap2::Mmap;
use rusqlite::Connection;

use crate::Result;

/// Fixed record: full commitment, stable ID, inline offset, payload and descriptor lengths.
const ROW_BYTES: usize = 56;
/// Two hash bytes keep binary searches within a small contiguous group.
const PREFIXES: usize = 65536;

/// Final leaf representation; absent commitments remain ordinary raw MARF values.
#[derive(Clone, Debug)]
pub enum Reference {
    /// A value outside the Clarity content-addressed store.
    Raw,
    /// Canonical inline payload and reconstruction descriptor.
    Inline(InlineValue),
    /// Nonzero stable value identifier.
    Stable(u32),
}

impl Reference {
    /// Apply only the physical representation, preserving the original logical value.
    pub fn apply(&self, leaf: &mut TrieLeaf) {
        match self {
            Self::Raw => {}
            Self::Inline(value) => leaf.inline = Some(value.clone()),
            Self::Stable(id) => leaf.value_id = Some(*id),
        }
    }
}

/// Read-only mappings shared by conversion workers; no per-leaf SQLite access.
pub struct ValueLookup {
    /// Sorted, immutable fixed records.
    rows: Option<Mmap>,
    /// Concatenated inline bytes, mapped only when nonempty.
    payload: Option<Mmap>,
    /// Start row for every two-byte prefix, followed by the total row count.
    starts: Vec<usize>,
}

/// Map a completed private file; it must remain immutable until all readers drop.
fn map(path: &Path) -> Result<Option<Mmap>> {
    let file = File::open(path)?;
    if file.metadata()?.len() == 0 {
        return Ok(None);
    }
    // SAFETY: these files are built exclusively, closed before mapping, and retained
    // unchanged by the converter until all lookup owners have been dropped.
    Ok(Some(unsafe { Mmap::map(&file)? }))
}

impl ValueLookup {
    /// Merge the existing covering indexes into one sequential mmap lookup.
    pub fn build(db: &Connection, directory: &Path) -> Result<Self> {
        let records = directory.join("value-lookup.scratch");
        let payload = directory.join("value-inline.scratch");
        let mut rows_out = BufWriter::with_capacity(
            1024 * 1024,
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&records)?,
        );
        let mut payload_out = BufWriter::with_capacity(
            1024 * 1024,
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&payload)?,
        );
        // UNION ALL ORDER BY merges the two pre-sorted covering indexes in SQLite.
        // It never runs a query for each leaf or inserts individual keys into a B-tree.
        let mut query = db.prepare(
            "SELECT hash,value_id,NULL,NULL FROM canonical_external_values
            UNION ALL SELECT hash,0,record,descriptor FROM canonical_inline_values ORDER BY hash",
        )?;
        let mut input = query.query([])?;
        let mut starts = vec![0usize; PREFIXES + 1];
        let mut next_prefix = 0;
        let mut count = 0usize;
        let mut bytes = 0u64;
        let mut previous: Option<[u8; 40]> = None;
        let mut space = crate::space::Guard::new(directory, 4 * 1024 * 1024 * 1024)?;
        while let Some(row) = input.next()? {
            let key: [u8; 40] = row.get_ref(0)?.as_blob()?.try_into()?;
            if previous.is_some_and(|old| old >= key) {
                return Err("duplicate or unordered extracted commitment".into());
            }
            let prefix = usize::from(u16::from_be_bytes([key[0], key[1]]));
            while next_prefix <= prefix {
                starts[next_prefix] = count;
                next_prefix += 1;
            }
            let id: u32 = row.get(1)?;
            let mut encoded = [0u8; ROW_BYTES];
            encoded[..40].copy_from_slice(&key);
            encoded[40..44].copy_from_slice(&id.to_le_bytes());
            if id == 0 {
                let record = row.get_ref(2)?.as_blob()?;
                let descriptor = row.get_ref(3)?.as_blob()?;
                if !InlineValue::fits_inline(record.len(), descriptor.len()) {
                    return Err("extracted value exceeds canonical inline policy".into());
                }
                InlineValue::from_parts(record, descriptor)?;
                binary_value_store::audit_stored_record(
                    &MARFValue(key),
                    record,
                    (!descriptor.is_empty()).then_some(descriptor),
                )?;
                encoded[44..52].copy_from_slice(&bytes.to_le_bytes());
                encoded[52..54].copy_from_slice(&u16::try_from(record.len())?.to_le_bytes());
                encoded[54..56].copy_from_slice(&u16::try_from(descriptor.len())?.to_le_bytes());
                payload_out.write_all(record)?;
                payload_out.write_all(descriptor)?;
                bytes = bytes
                    .checked_add((record.len() + descriptor.len()) as u64)
                    .ok_or("inline scratch overflow")?;
            }
            rows_out.write_all(&encoded)?;
            count = count.checked_add(1).ok_or("lookup row overflow")?;
            previous = Some(key);
            if count % 1_000_000 == 0 {
                space.check()?;
                eprintln!("canonical mapping_value_lookups={count}");
            }
        }
        starts[next_prefix..].fill(count);
        rows_out.flush()?;
        payload_out.flush()?;
        drop(rows_out);
        drop(payload_out);
        let rows = map(&records)?;
        let payload = map(&payload)?;
        Ok(Self {
            rows,
            payload,
            starts,
        })
    }

    /// Resolve the full 40-byte key, never treating a prefix match as membership.
    pub fn get(&self, key: &MARFValue) -> std::result::Result<Reference, MarfError> {
        let prefix = usize::from(u16::from_be_bytes([key.0[0], key.0[1]]));
        let (mut low, mut high) = (self.starts[prefix], self.starts[prefix + 1]);
        let Some(rows) = &self.rows else {
            return Ok(Reference::Raw);
        };
        while low < high {
            let mid = low + (high - low) / 2;
            let row = &rows[mid * ROW_BYTES..(mid + 1) * ROW_BYTES];
            match row[..40].cmp(&key.0) {
                std::cmp::Ordering::Less => low = mid + 1,
                std::cmp::Ordering::Greater => high = mid,
                std::cmp::Ordering::Equal => {
                    let id = u32::from_le_bytes(row[40..44].try_into().expect("fixed record"));
                    if id != 0 {
                        return Ok(Reference::Stable(id));
                    }
                    let offset = usize::try_from(u64::from_le_bytes(
                        row[44..52].try_into().expect("fixed record"),
                    ))
                    .map_err(|_| MarfError::OverflowError)?;
                    let len = usize::from(u16::from_le_bytes(
                        row[52..54].try_into().expect("fixed record"),
                    ));
                    let descriptor = usize::from(u16::from_le_bytes(
                        row[54..56].try_into().expect("fixed record"),
                    ));
                    let end = offset
                        .checked_add(len)
                        .and_then(|n| n.checked_add(descriptor))
                        .ok_or(MarfError::OverflowError)?;
                    let bytes = self
                        .payload
                        .as_deref()
                        .unwrap_or(&[])
                        .get(offset..end)
                        .ok_or_else(|| {
                            MarfError::CorruptionError("inline lookup out of bounds".into())
                        })?;
                    return Ok(Reference::Inline(InlineValue::from_parts(
                        &bytes[..len],
                        &bytes[len..],
                    )?));
                }
            }
        }
        Ok(Reference::Raw)
    }

    /// Number of unique extracted commitments, including inline values.
    pub fn len(&self) -> usize {
        self.starts[PREFIXES]
    }

    /// Remove scratch only after every shared mapping and worker has been released.
    pub fn remove_files(directory: &Path) -> Result<()> {
        for name in ["value-lookup.scratch", "value-inline.scratch"] {
            fs::remove_file(directory.join(name))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::assert_matches;

    use rusqlite::params;

    use super::*;
    use crate::memberships;

    /// Sparse prefixes, full-key misses and both leaf classes preserve exact references.
    #[test]
    fn mapped_lookup_matches_extracted_values() {
        let db = Connection::open_in_memory().unwrap();
        memberships::initialize(&db).unwrap();
        for n in [0u8, 1, 17, 255] {
            let hash = [n; 40];
            db.execute(
                "INSERT INTO canonical_external_values VALUES(?1,?2)",
                params![hash.as_slice(), u32::from(n) + 1],
            )
            .unwrap();
        }
        let inline = MARFValue::from_value("tiny");
        let encoded = binary_value_store::encode_migrated("tiny").unwrap();
        db.execute(
            "INSERT INTO canonical_inline_values VALUES(?1,?2,?3)",
            params![
                inline.0.as_slice(),
                encoded.record(),
                encoded.shape().unwrap_or_default()
            ],
        )
        .unwrap();
        memberships::index(&db).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let map = ValueLookup::build(&db, dir.path()).unwrap();
        for n in [0u8, 1, 17, 255] {
            assert_matches!(map.get(&MARFValue([n; 40])).unwrap(), Reference::Stable(id) if id == u32::from(n) + 1);
            let mut missing = [n; 40];
            missing[39] ^= 1;
            assert_matches!(map.get(&MARFValue(missing)).unwrap(), Reference::Raw);
        }
        let Reference::Inline(value) = map.get(&inline).unwrap() else {
            panic!("inline");
        };
        assert_eq!(value.record(), encoded.record());
        assert!(value.descriptor().is_empty());
        drop(map);
        ValueLookup::remove_files(dir.path()).unwrap();
    }

    /// Empty input is valid; a key shared by the two tables fails before use.
    #[test]
    fn empty_and_conflicting_lookup() {
        let db = Connection::open_in_memory().unwrap();
        memberships::initialize(&db).unwrap();
        let dir = tempfile::tempdir().unwrap();
        assert_matches!(
            ValueLookup::build(&db, dir.path())
                .unwrap()
                .get(&MARFValue([0; 40]))
                .unwrap(),
            Reference::Raw
        );
        db.execute_batch("INSERT INTO canonical_external_values VALUES(zeroblob(40),1); INSERT INTO canonical_inline_values VALUES(zeroblob(40),x'01',x'')").unwrap();
        let duplicate = tempfile::tempdir().unwrap();
        assert!(ValueLookup::build(&db, duplicate.path()).is_err());
    }
}
