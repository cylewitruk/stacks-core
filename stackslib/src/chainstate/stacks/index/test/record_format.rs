//! Persistent type-first format coverage across storage backends and mutable reopens.

use std::path::Path;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use tempfile::tempdir;

use super::*;
use crate::chainstate::stacks::index::direct_hash_index;
use crate::chainstate::stacks::index::inline_value::InlineValue;
use crate::chainstate::stacks::index::record::NodeRecordFormat;
use crate::chainstate::stacks::index::{ClarityMarfTrieId, ValueResolver};

/// Count requests to reconstruct the commitment of a known stable-ID value.
struct CountingResolver {
    /// Number of explicit commitment requests.
    calls: AtomicUsize,
}

impl ValueResolver for CountingResolver {
    fn inline_commitment(&self, _: &InlineValue) -> Result<MARFValue, Error> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        Ok(MARFValue::from_value("value"))
    }
    fn commitment_by_id(&self, id: u32) -> Result<MARFValue, Error> {
        assert_eq!(id, 7);
        self.calls.fetch_add(1, Ordering::Relaxed);
        Ok(MARFValue::from_value("value"))
    }
}

/// A locator lookup must not hash the value; logical reads and mutable loading may resolve it.
#[test]
fn compact_leaf_reads_and_unconfirmed_reopens() {
    compact_leaf_reopens(false, false);
}

/// Optional direct hashes preserve compact extent reads and unconfirmed fallback behavior.
#[test]
fn compact_leaf_direct_hash_reads_and_unconfirmed_reopens() {
    compact_leaf_reopens(true, false);
}

/// Run mapped and ordinary compact-leaf reads against the requested index configuration.
fn compact_leaf_reopens(indexed: bool, inline: bool) {
    let format = NodeRecordFormat::Optimized;
    for external in [true] {
        if indexed && !external {
            continue;
        }
        for compression in [false, true] {
            for mmap in [false, true] {
                let directory = tempdir().unwrap();
                let path = directory.path().join("index.sqlite");
                let path = path.to_str().unwrap();
                let mut opts = MARFOpenOpts::default()
                    .with_compression(compression)
                    .with_mmap(mmap);
                opts.external_blobs = external;
                let resolver = Arc::new(CountingResolver {
                    calls: AtomicUsize::new(0),
                });
                let value_leaf = || {
                    let mut leaf = TrieLeaf::from_value(&[], MARFValue::from_value("value"));
                    if inline {
                        leaf.inline = Some(InlineValue::from_parts(b"value", &[]).unwrap());
                    } else {
                        leaf.value_id = Some(7);
                    }
                    leaf
                };
                let block = StacksBlockId([22; 32]);
                let mut marf = MARF::<StacksBlockId>::from_path(path, opts.clone()).unwrap();
                format.publish(marf.sqlite_conn()).unwrap();
                marf.set_record_format(format);
                marf.set_value_resolver(resolver.clone());
                {
                    let mut tx = marf.begin_tx().unwrap();
                    tx.begin(&StacksBlockId::sentinel(), &block).unwrap();
                    tx.insert_leaf_batch(
                        &["key".into(), "other".into()],
                        vec![value_leaf(), value_leaf()],
                    )
                    .unwrap();
                    tx.commit().unwrap();
                }
                drop(marf);
                if indexed {
                    direct_hash_index::build(Path::new(path)).unwrap();
                }
                let mut marf = MARF::<StacksBlockId>::from_storage(
                    TrieFileStorage::open_readonly(path, opts.clone()).unwrap(),
                );
                marf.set_value_resolver(resolver.clone());
                resolver.calls.store(0, Ordering::Relaxed);
                let leaf = marf
                    .get_leaf(&block, &TrieHash::from_key("key"))
                    .unwrap()
                    .unwrap();
                if inline {
                    assert_eq!(leaf.inline.as_ref().unwrap().record(), b"value");
                } else {
                    assert_eq!(leaf.value_id, Some(7));
                }
                assert_eq!(leaf.data, None);
                assert_eq!(resolver.calls.load(Ordering::Relaxed), 0);
                assert_eq!(
                    marf.get(&block, "key").unwrap(),
                    Some(MARFValue::from_value("value"))
                );
                assert!(resolver.calls.load(Ordering::Relaxed) > 0);
                drop(marf);

                let mut unconfirmed =
                    MARF::<StacksBlockId>::from_path_unconfirmed(path, opts.clone()).unwrap();
                unconfirmed.set_value_resolver(resolver.clone());
                let tip = {
                    let mut tx = unconfirmed.begin_tx().unwrap();
                    let tip = tx.begin_unconfirmed(&block).unwrap();
                    tx.insert_leaf_batch(&["key".into()], vec![value_leaf()])
                        .unwrap();
                    tx.commit().unwrap();
                    tip
                };
                drop(unconfirmed);
                let mut reopened =
                    MARF::<StacksBlockId>::from_path_unconfirmed(path, opts).unwrap();
                reopened.set_value_resolver(resolver);
                {
                    let mut tx = reopened.begin_tx().unwrap();
                    assert_eq!(tx.begin_unconfirmed(&block).unwrap(), tip);
                    tx.insert_leaf_batch(&["new".into()], vec![value_leaf()])
                        .unwrap();
                    tx.commit().unwrap();
                }
                for key in ["key", "other", "new"] {
                    assert_eq!(
                        reopened.get(&tip, key).unwrap(),
                        Some(MARFValue::from_value("value"))
                    );
                    let (value, _) = reopened.get_with_proof(&tip, key).unwrap().unwrap();
                    assert_eq!(value, MARFValue::from_value("value"));
                }
            }
        }
    }
}

/// Physical leaf widths must not change any committed root or serialized proof across forks.
#[test]
fn compact_raw_roots_and_proofs_match_legacy() {
    for mmap in [false, true] {
        for compression in [false, true] {
            let mut expected = None;
            for format in [NodeRecordFormat::Legacy, NodeRecordFormat::Optimized] {
                let directory = tempdir().unwrap();
                let path = directory.path().join("marf.sqlite");
                let mut opts = MARFOpenOpts::default()
                    .with_mmap(mmap)
                    .with_compression(compression);
                opts.external_blobs = true;
                let mut marf =
                    MARF::<StacksBlockId>::from_path(path.to_str().unwrap(), opts.clone()).unwrap();
                if format.is_type_first() {
                    format.publish(marf.sqlite_conn()).unwrap();
                    marf.set_record_format(format);
                }
                let mut observed = Vec::new();
                for block in 1u8..=18 {
                    let parent = if block == 1 {
                        StacksBlockId::sentinel()
                    } else {
                        StacksBlockId([if block % 7 == 0 { block - 4 } else { block - 1 }; 32])
                    };
                    let id = StacksBlockId([block; 32]);
                    let mut tx = marf.begin_tx().unwrap();
                    tx.begin(&parent, &id).unwrap();
                    for n in 0..96 {
                        let mut value = [0; 40];
                        let width = [0, 4, 32, 40][n % 4];
                        value[..width].fill(block.wrapping_add(n as u8));
                        tx.insert_batch(&[format!("key-{n}")], vec![MARFValue(value)])
                            .unwrap();
                    }
                    tx.commit().unwrap();
                    if block % 3 == 0 {
                        drop(marf);
                        marf = MARF::from_path(path.to_str().unwrap(), opts.clone()).unwrap();
                    }
                    let root = marf.get_root_hash_at(&id).unwrap();
                    let mut proofs = Vec::new();
                    for n in 0..4 {
                        let (value, proof) = match marf.get_with_proof(&id, &format!("key-{n}")) {
                            Ok(value) => value.unwrap(),
                            Err(error) => {
                                let saved = directory.into_path();
                                panic!(
                                    "{format:?} mmap={mmap} compression={compression} block={block} key={n}: {error:?}; saved {}",
                                    saved.display()
                                );
                            }
                        };
                        proofs.push((value, proof.to_hex()));
                    }
                    observed.push((root, proofs));
                }
                if let Some(expected) = &expected {
                    assert_eq!(&observed, expected);
                } else {
                    expected = Some(observed);
                }
            }
        }
    }
}

/// Retired and incomplete markers cannot be read or accidentally republished.
#[test]
fn canonical_metadata_and_header_guards() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("guard.sqlite");
    let marf =
        MARF::<StacksBlockId>::from_path(path.to_str().unwrap(), MARFOpenOpts::default()).unwrap();
    let db = marf.sqlite_conn();
    let format = NodeRecordFormat::Optimized;
    format.publish(db).unwrap();
    assert_eq!(NodeRecordFormat::from_database(db).unwrap(), format);
    assert!(NodeRecordFormat::Legacy.publish(db).is_err());
    for version in [1, 2, 3, 4, 5, 41, -1, -2, -3, -4, -5, -41, 999] {
        db.execute("UPDATE marf_record_format SET version=?1", [version])
            .unwrap();
        assert!(NodeRecordFormat::from_database(db).is_err());
        assert!(format.publish(db).is_err());
    }
    db.execute("UPDATE marf_record_format SET version=-6", [])
        .unwrap();
    assert!(NodeRecordFormat::from_database(db).is_err());
    format.publish(db).unwrap();
    let mut header = Vec::new();
    format
        .write_trie_header(&mut header, &StacksBlockId::sentinel())
        .unwrap();
    format.validate_trie_header(&header).unwrap();
    for version in [1, 2, 3, 4, 5, 41, 255] {
        header[35] = version;
        assert!(format.validate_trie_header(&header).is_err());
    }
}

/// Inline owners survive mapped/buffered reads, direct indexes and unconfirmed reopens.
#[test]
fn inline_leaf_reads_and_unconfirmed_reopens() {
    compact_leaf_reopens(false, true);
    compact_leaf_reopens(true, true);
}
