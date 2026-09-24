//! Physical leaf transformation independent of ordered migration and publication.
use std::path::{Path, PathBuf};

use blockstack_lib::chainstate::stacks::index::record::NodeRecordFormat;
use blockstack_lib::chainstate::stacks::index::{Error, TrieLeaf};
use blockstack_lib::clarity_vm::database::value_extents::ValueExtentStore;

/// Source layout admitted by this immutable converter version.
pub const SOURCE_FORMAT: NodeRecordFormat = NodeRecordFormat::TypeFirstV2;
/// Published destination layout.
pub const DESTINATION_FORMAT: NodeRecordFormat = NodeRecordFormat::TypeFirstV3;
/// Planning identity changes whenever transformation policy changes.
pub const BINDING: &str = "inline-packed-v3-production-plan-1";

/// Physical leaf-reference counts; these are not unique or live value counts.
#[derive(Default)]
pub struct Counts {
    /// Source extent leaves inspected.
    pub extents: u64,
    /// Leaves converted to inline payloads.
    pub inlined: u64,
    /// Combined inline record and descriptor bytes, excluding two framing bytes.
    pub inline_bytes: u64,
}

/// One shared immutable values mapping used by every transformation worker.
pub struct Codec {
    /// Read-only extent generation, never appended during conversion.
    values: ValueExtentStore,
}

impl Codec {
    /// Open the original values generation without creating or mutating it.
    pub fn open(db: &Path) -> crate::migration::Result<Self> {
        let values = PathBuf::from(format!("{}.values", db.display()));
        Ok(Self {
            values: ValueExtentStore::open_existing(&values, false).map_err(|e| e.to_string())?,
        })
    }

    /// Replace only eligible locators; preserve raw values and all logical commitments.
    pub fn transform(&self, leaf: &mut TrieLeaf, counts: &mut Counts) -> Result<(), Error> {
        let Some(extent) = leaf.extent else {
            return Ok(());
        };
        counts.extents += 1;
        let record = self
            .values
            .read_at(extent)
            .map_err(|e| Error::CorruptionError(e.to_string()))?;
        if let Some(inline) = record
            .inline_candidate()
            .map_err(|e| Error::CorruptionError(e.to_string()))?
        {
            counts.inlined += 1;
            counts.inline_bytes += (inline.record().len() + inline.descriptor().len()) as u64;
            leaf.inline = Some(inline);
            leaf.extent = None;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use blockstack_lib::chainstate::stacks::index::MARFValue;
    use blockstack_lib::chainstate::stacks::index::inline_value::InlineValue;
    use blockstack_lib::clarity_vm::database::value_extents::InlineValueRecord;
    use clarity::vm::database::DataStoreValue;

    /// Shared workers select exact encoded-size boundaries and leave the value generation untouched.
    #[test]
    fn mixed_extents_inline_exactly_and_read_concurrently() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("marf.sqlite");
        let path = dir.path().join("marf.sqlite.values");
        let values: Vec<_> = (0..=100)
            .map(|n| DataStoreValue::Canonical("x".repeat(n)))
            .collect();
        let mut store = ValueExtentStore::open(&path, true).unwrap();
        let entries = store.append(&values).unwrap();
        drop(store);
        let before = std::fs::read(&path).unwrap();
        let codec = Codec::open(&db).unwrap();
        std::thread::scope(|scope| {
            for _ in 0..8 {
                let codec = &codec;
                let entries = &entries;
                let values = &values;
                scope.spawn(move || {
                    let mut counts = Counts::default();
                    for ((hash, extent), value) in entries.iter().zip(values) {
                        let source_record = codec.values.read_at(*extent).unwrap();
                        let candidate = source_record.inline_candidate().unwrap();
                        let mut leaf = TrieLeaf::from_value(&[], hash.clone());
                        leaf.data = None;
                        leaf.extent = Some(*extent);
                        codec.transform(&mut leaf, &mut counts).unwrap();
                        assert_eq!(leaf.inline, candidate);
                        assert!(leaf.data.is_none());
                        if let Some(inline) = &leaf.inline {
                            assert!(InlineValue::fits_inline(
                                inline.record().len(),
                                inline.descriptor().len()
                            ));
                            assert!(leaf.extent.is_none());
                            assert_eq!(
                                InlineValueRecord::from_inline(inline).canonical().unwrap(),
                                value.canonical()
                            );
                            assert_eq!(
                                InlineValueRecord::from_inline(inline).commitment().unwrap(),
                                *hash
                            );
                        } else {
                            assert_eq!(leaf.extent, Some(*extent));
                        }
                    }
                    assert_eq!(counts.extents, 101);
                    assert_eq!(counts.inlined, 29); // two envelope bytes plus0..28textbytes
                    let mut invalid = TrieLeaf::from_value(&[], MARFValue([0; 40]));
                    let mut wrong_generation = entries[0].1;
                    wrong_generation.store_id[0] ^= 1;
                    invalid.extent = Some(wrong_generation);
                    assert!(codec.transform(&mut invalid, &mut counts).is_err());
                    assert_eq!(invalid.extent, Some(wrong_generation));
                    assert!(invalid.inline.is_none());
                });
            }
        });
        assert_eq!(before, std::fs::read(&path).unwrap());
    }
}
