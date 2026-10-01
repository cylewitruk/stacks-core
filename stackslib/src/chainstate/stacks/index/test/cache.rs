//! Correctness checks for the small committed-root cache.

use stacks_common::codec::StacksMessageCodec;
use stacks_common::types::chainstate::StacksBlockId;
use stacks_common::types::chainstate::TrieHash;

use crate::chainstate::stacks::index::cache::{ArrayLru, SharedLru, SmallLru};
use crate::chainstate::stacks::index::marf::{MARF, MARFOpenOpts, MarfConnection, MarfCore};
use crate::chainstate::stacks::index::scratch::MarfReadState;
use crate::chainstate::stacks::index::storage::TrieFileStorage;
use crate::chainstate::stacks::index::{ClarityMarfTrieId, MARFValue, TrieLeaf, TrieReadStorage};
use std::sync::{Arc, Barrier};
use std::thread;

/// Point writes, raw writes and batches evict changed values and cached absence only.
#[test]
fn result_cache_updates_preserve_unrelated_keys() {
    let mut marf =
        MARF::from_storage(TrieFileStorage::new_memory(MARFOpenOpts::default()).unwrap());
    let tip = StacksBlockId([71; 32]);
    marf.begin(&StacksBlockId::sentinel(), &tip).unwrap();
    marf.insert("a", MARFValue::from(1)).unwrap();
    marf.insert("b", MARFValue::from(2)).unwrap();
    assert_eq!(marf.get(&tip, "a").unwrap(), Some(MARFValue::from(1)));
    assert_eq!(marf.get(&tip, "b").unwrap(), Some(MARFValue::from(2)));
    assert_eq!(marf.get(&tip, "missing").unwrap(), None);
    let a = TrieHash::from_key("a");
    let b = TrieHash::from_key("b");
    let missing = TrieHash::from_key("missing");
    marf.insert("a", MARFValue::from(3)).unwrap();
    {
        let mut storage = marf.borrow_storage_backend();
        assert_eq!(storage.cached_result(&tip, &a), None);
        assert_eq!(
            storage
                .cached_result(&tip, &b)
                .map(|leaf| leaf.map(|leaf| leaf.data.expect("resolved cached leaf"))),
            Some(Some(MARFValue::from(2)))
        );
        assert_eq!(storage.cached_result(&tip, &missing), Some(None));
    }
    assert_eq!(
        marf.get_from_hash(&tip, &a).unwrap(),
        Some(MARFValue::from(3))
    );
    marf.insert_raw(missing, TrieLeaf::from_value(&[], MARFValue::from(4)))
        .unwrap();
    assert_eq!(
        marf.borrow_storage_backend().cached_result(&tip, &missing),
        None
    );
    assert_eq!(marf.get(&tip, "missing").unwrap(), Some(MARFValue::from(4)));
    marf.insert_batch(
        &["a".into(), "missing".into()],
        &[MARFValue::from(5), MARFValue::from(6)],
    )
    .unwrap();
    assert_eq!(
        marf.borrow_storage_backend()
            .cached_result(&tip, &b)
            .map(|leaf| leaf.map(|leaf| leaf.data.expect("resolved cached leaf"))),
        Some(Some(MARFValue::from(2)))
    );
    assert_eq!(marf.get(&tip, "a").unwrap(), Some(MARFValue::from(5)));
    assert_eq!(marf.get(&tip, "missing").unwrap(), Some(MARFValue::from(6)));
    // Proving traverses the trie independently of the complete-result cache.
    assert_eq!(
        marf.get_with_proof(&tip, "a").unwrap().unwrap().0,
        MARFValue::from(5)
    );
}

/// State replacement, historical reads, reopen and explicit/implicit rollback are isolated.
#[test]
fn result_cache_state_lifetimes_and_rollback() {
    let mut marf =
        MARF::from_storage(TrieFileStorage::new_memory(MARFOpenOpts::default()).unwrap());
    let first = StacksBlockId([72; 32]);
    let next = StacksBlockId([73; 32]);
    let key = TrieHash::from_key("a");
    marf.begin(&StacksBlockId::sentinel(), &first).unwrap();
    marf.insert("a", MARFValue::from(1)).unwrap();
    marf.get(&first, "a").unwrap();
    // A dropped storage transaction must discard results even without an explicit rollback.
    {
        let _tx = marf.borrow_storage_transaction();
    }
    assert_eq!(marf.borrow_storage_backend().result_cache_len(), 0);
    marf.get(&first, "a").unwrap();
    marf.borrow_storage_transaction().rollback();
    assert_eq!(marf.borrow_storage_backend().result_cache_len(), 0);
    marf.get(&first, "a").unwrap();
    marf.commit().unwrap();
    {
        let mut reopened = marf.reopen_connection().unwrap();
        assert_eq!(reopened.connection().result_cache_len(), 0);
    }
    assert_eq!(
        marf.borrow_storage_backend().cached_result(&first, &key),
        None
    );
    marf.begin(&first, &next).unwrap();
    marf.insert("a", MARFValue::from(2)).unwrap();
    assert_eq!(marf.get(&next, "a").unwrap(), Some(MARFValue::from(2)));
    assert_eq!(marf.get(&first, "a").unwrap(), Some(MARFValue::from(1)));
    {
        let mut storage = marf.borrow_storage_backend();
        assert_eq!(storage.cached_result(&first, &key), None);
        assert_eq!(
            storage
                .cached_result(&next, &key)
                .map(|leaf| leaf.map(|leaf| leaf.data.expect("resolved cached leaf"))),
            Some(Some(MARFValue::from(2)))
        );
    }
    marf.drop_current();
    marf.begin(&first, &next).unwrap();
    assert_eq!(
        marf.borrow_storage_backend().cached_result(&next, &key),
        None
    );
    assert_eq!(marf.get(&next, "a").unwrap(), Some(MARFValue::from(1)));
}

/// Cached mutable-state reads match uncached values, proofs and roots across storage modes.
#[test]
fn result_cache_matches_uncached_mutations_and_proofs() {
    for (external_blobs, mmap) in [(false, false), (true, false), (true, true)] {
        let dir = tempfile::tempdir().unwrap();
        let mut opts = MARFOpenOpts::default()
            .with_compression(true)
            .with_mmap(mmap);
        opts.external_blobs = external_blobs;
        opts.result_cache_capacity = 16;
        let mut cached = MARF::from_path(
            dir.path().join("cached.sqlite").to_str().unwrap(),
            opts.clone(),
        )
        .unwrap();
        opts.result_cache_capacity = 0;
        let mut uncached =
            MARF::from_path(dir.path().join("uncached.sqlite").to_str().unwrap(), opts).unwrap();
        let mut parent = StacksBlockId::sentinel();
        for height in 1..=5u8 {
            let tip = StacksBlockId([height; 32]);
            cached.begin(&parent, &tip).unwrap();
            uncached.begin(&parent, &tip).unwrap();
            for step in 0..64u32 {
                let key = format!("key-{}", (step * 7) % 23);
                for _ in 0..2 {
                    assert_eq!(
                        cached.get(&tip, &key).unwrap(),
                        uncached.get(&tip, &key).unwrap()
                    );
                }
                let value = MARFValue::from(u32::from(height) * 1000 + step);
                cached.insert(&key, value.clone()).unwrap();
                uncached.insert(&key, value.clone()).unwrap();
                assert_eq!(cached.get(&tip, &key).unwrap(), Some(value));
                let proof = cached.get_with_proof(&tip, &key).unwrap().unwrap();
                let expected = uncached.get_with_proof(&tip, &key).unwrap().unwrap();
                assert_eq!(proof.0, expected.0);
                assert_eq!(proof.1.serialize_to_vec(), expected.1.serialize_to_vec());
                assert!(cached.borrow_storage_backend().result_cache_len() <= 16);
            }
            cached.commit().unwrap();
            uncached.commit().unwrap();
            assert_eq!(
                cached.get_root_hash_at(&tip).unwrap(),
                uncached.get_root_hash_at(&tip).unwrap()
            );
            parent = tip;
        }
    }
}

/// An unconfirmed identifier can be reused only with a fresh cache for the reloaded state.
#[test]
fn result_cache_unconfirmed_reload() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("marf.sqlite");
    let path = path.to_str().unwrap();
    let tip = {
        let mut confirmed = MARF::from_path(path, MARFOpenOpts::default()).unwrap();
        populate(&mut confirmed, 1).pop().unwrap()
    };
    let mut marf = MARF::from_storage(
        TrieFileStorage::open_unconfirmed(path, MARFOpenOpts::default()).unwrap(),
    );
    for value in [1u32, 2, 3] {
        let active = marf.begin_unconfirmed(&tip).unwrap();
        assert_eq!(
            marf.borrow_storage_backend()
                .cached_result(&active, &TrieHash::from_key("mutable")),
            None
        );
        marf.get(&active, "mutable").unwrap();
        marf.insert("mutable", MARFValue::from(value)).unwrap();
        assert_eq!(
            marf.get(&active, "mutable").unwrap(),
            Some(MARFValue::from(value))
        );
        marf.commit().unwrap();
        assert_eq!(
            marf.get(&active, "mutable").unwrap(),
            Some(MARFValue::from(value))
        );
        assert_eq!(marf.borrow_storage_backend().result_cache_len(), 0);
    }
}

/// Known-height resolution matches normal resolution and skips its extra trie read.
#[test]
fn ancestor_resolution_reuses_current_height() {
    let mut opts = MARFOpenOpts::default();
    opts.result_cache_capacity = 0;
    let mut marf = MARF::from_storage(TrieFileStorage::new_memory(opts).unwrap());
    let tips = populate(&mut marf, 8);
    let tip = tips.last().unwrap();
    marf.with_read_ctx(|ctx| {
        let height = ctx.get_block_height(tip, tip).unwrap().unwrap();
        assert!(height > 1);
        for target in 0..=height + 1 {
            ctx.storage().open_block(tip).unwrap();
            ctx.storage().stats();
            let expected = ctx.get_block_at_height(target, tip).unwrap();
            let normal_reads = ctx.storage().stats().0;
            let actual = ctx
                .get_block_at_height_with_current_height(target, tip, height)
                .unwrap();
            let known_height_reads = ctx.storage().stats().0;
            assert_eq!(actual, expected);
            if target > 0 {
                assert!(known_height_reads < normal_reads);
            }
        }
    });
}

/// Access promotes entries before capacity eviction.
#[test]
fn lru_get_promotes_and_affects_eviction() {
    let mut cache = ArrayLru::<u32, &'static str, 3>::new();
    cache.put(1, "one");
    cache.put(2, "two");
    cache.put(3, "three");
    // State: [3(MRU), 2, 1(LRU)]

    // Promote key 1 to MRU via rotate_right: [3,2,1] -> [1,3,2].
    assert_eq!(cache.get(&1), Some(&"one"));
    // State: [1(MRU), 3, 2(LRU)]

    // Next insert evicts the true LRU (key 2), leaving 1 and 3.
    cache.put(4, "four");
    assert_eq!(cache.get(&2), None);
    assert_eq!(cache.get(&1), Some(&"one"));
    assert_eq!(cache.get(&3), Some(&"three"));
    assert_eq!(cache.get(&4), Some(&"four"));
}

/// Replacing a cached value also promotes its entry.
#[test]
fn lru_put_existing_updates_and_promotes() {
    let mut cache = ArrayLru::<u32, u32, 3>::new();
    cache.put(1, 10);
    cache.put(2, 20);
    cache.put(3, 30);

    // Update key 2 and promote it to MRU.
    cache.put(2, 200);
    assert_eq!(cache.get(&2), Some(&200));

    // LRU should now be key 1.
    cache.put(4, 40);
    assert_eq!(cache.get(&1), None);
    assert_eq!(cache.get(&2), Some(&200));
    assert_eq!(cache.get(&3), Some(&30));
    assert_eq!(cache.get(&4), Some(&40));
}

/// Clearing drops all entries and permits reuse.
#[test]
fn lru_clear_resets_state() {
    let mut cache = ArrayLru::<u32, u32, 3>::new();
    cache.put(1, 10);
    cache.put(2, 20);
    cache.put(3, 30);
    cache.clear();

    assert_eq!(cache.get(&1), None);
    assert_eq!(cache.get(&2), None);
    assert_eq!(cache.get(&3), None);

    // Reusable after clear.
    cache.put(9, 90);
    assert_eq!(cache.get(&9), Some(&90));
}

/// A single-entry cache replaces and updates correctly.
#[test]
fn lru_capacity_one() {
    let mut cache = ArrayLru::<u8, u8, 1>::new();
    cache.put(1, 10);
    assert_eq!(cache.get(&1), Some(&10));

    cache.put(2, 20);
    assert_eq!(cache.get(&1), None);
    assert_eq!(cache.get(&2), Some(&20));

    cache.put(2, 22);
    assert_eq!(cache.get(&2), Some(&22));
}

/// Build committed tries with distinct roots and a stable inherited key.
fn populate(marf: &mut MARF<StacksBlockId>, count: u8) -> Vec<StacksBlockId> {
    let mut parent = StacksBlockId::sentinel();
    let mut tips = Vec::new();
    for i in 1..=count {
        let tip = StacksBlockId([i; 32]);
        marf.begin(&parent, &tip).unwrap();
        marf.insert(
            &format!("key-{i}"),
            MARFValue::from_value(&format!("value-{i}")),
        )
        .unwrap();
        marf.commit().unwrap();
        tips.push(tip.clone());
        parent = tip;
    }
    tips
}

/// Cached roots retain node hashes and bounded eviction while preserving historical values.
#[test]
fn committed_root_cache_eviction_hashes_and_reopen() {
    let opts = MARFOpenOpts::default().with_compression(true);
    let mut marf = MARF::from_storage(TrieFileStorage::new_memory(opts).unwrap());
    let tips = populate(&mut marf, 6);
    let expected: Vec<_> = tips
        .iter()
        .map(|tip| marf.get_root_hash_at(tip).unwrap())
        .collect();
    let mut state = MarfReadState::new();
    {
        let mut storage = marf.borrow_storage_backend();
        for (i, tip) in tips.iter().enumerate() {
            storage.open_block(tip).unwrap();
            let ptr = storage.root_trieptr();
            let first = storage
                .read_node_with_state(&ptr, &mut state)
                .unwrap()
                .into_owned_node()
                .unwrap();
            let second = storage
                .read_node_with_state(&ptr, &mut state)
                .unwrap()
                .into_owned_node()
                .unwrap();
            assert_eq!(first, second);
            assert_eq!(first.1, Some(expected[i]));
            assert!(storage.root_node_cache_has_current_block());
            assert!(storage.root_node_cache_len() <= 4);
        }
        storage.open_block(&tips[0]).unwrap();
        assert!(!storage.root_node_cache_has_current_block());
        storage.open_block(&tips[5]).unwrap();
    }
    {
        let mut reopened = marf.reopen_connection().unwrap();
        assert!(reopened.connection().root_node_cache_has_current_block());
        for tip in &tips {
            assert_eq!(
                reopened.get(tip, "key-1").unwrap(),
                Some(MARFValue::from_value("value-1"))
            );
        }
    }
    let discarded = StacksBlockId([99; 32]);
    marf.begin(&tips[5], &discarded).unwrap();
    marf.insert("discarded", MARFValue::from_value("discarded"))
        .unwrap();
    marf.drop_current();
    assert_eq!(marf.borrow_storage_backend().root_node_cache_len(), 0);
    assert_eq!(marf.get(&tips[5], "discarded").unwrap(), None);
}

/// Rewriting persisted unconfirmed state must never serve a stale cached root.
#[test]
fn unconfirmed_root_cache_bypass_after_rewrite() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("marf.sqlite");
    let path = path.to_str().unwrap();
    let tip = {
        let mut marf =
            MARF::from_storage(TrieFileStorage::open(path, MARFOpenOpts::default()).unwrap());
        populate(&mut marf, 1).pop().unwrap()
    };
    let mut marf = MARF::from_storage(
        TrieFileStorage::open_unconfirmed(path, MARFOpenOpts::default()).unwrap(),
    );
    for value in ["first", "second"] {
        let unconfirmed = marf.begin_unconfirmed(&tip).unwrap();
        marf.insert("mutable", MARFValue::from_value(value))
            .unwrap();
        marf.commit().unwrap();
        assert_eq!(
            marf.get(&unconfirmed, "mutable").unwrap(),
            Some(MARFValue::from_value(value))
        );
        let mut storage = marf.borrow_storage_backend();
        storage.open_block(&unconfirmed).unwrap();
        let ptr = storage.root_trieptr();
        storage
            .read_node_with_state(&ptr, &mut MarfReadState::new())
            .unwrap();
        assert!(!storage.root_node_cache_has_current_block());
    }
}

/// Dynamic capacity, promotion, updates, clearing and disabled admission are bounded.
#[test]
fn resolved_lru_capacity_and_eviction() {
    let mut cache = SmallLru::new(2);
    cache.put(1, 10);
    cache.put(2, 20);
    assert_eq!(cache.get(&1), Some(&10));
    cache.put(3, 30);
    assert!(!cache.contains_key(&2));
    cache.put(1, 11);
    assert_eq!(cache.get(&1), Some(&11));
    assert_eq!(cache.len(), 2);
    cache.clear();
    assert_eq!(cache.len(), 0);
    let mut disabled = SmallLru::new(0);
    disabled.put(1, 10);
    assert_eq!(disabled.len(), 0);
}

/// Cached non-root patches must preserve values, commitment hashes, proofs and later writes.
#[test]
fn resolved_patch_cache_matches_uncached_history() {
    for (external_blobs, mmap) in [(false, false), (true, false), (true, true)] {
        let dir = tempfile::tempdir().unwrap();
        let mut opts = MARFOpenOpts::default()
            .with_compression(true)
            .with_mmap(mmap);
        opts.external_blobs = external_blobs;
        opts.resolved_patch_cache_capacity = 4;
        let cached_path = dir.path().join("cached.sqlite");
        let mut cached = MARF::from_storage(
            TrieFileStorage::open(cached_path.to_str().unwrap(), opts.clone()).unwrap(),
        );
        opts.resolved_patch_cache_capacity = 0;
        let plain_path = dir.path().join("plain.sqlite");
        let mut plain = MARF::from_storage(
            TrieFileStorage::open(plain_path.to_str().unwrap(), opts.clone()).unwrap(),
        );
        let mut parent = StacksBlockId::sentinel();
        let mut saw_nonroot_cache = false;
        for block in 1..=12u8 {
            let tip = StacksBlockId([block; 32]);
            for marf in [&mut cached, &mut plain] {
                marf.begin(&parent, &tip).unwrap();
                let count = if block == 1 { 512 } else { 24 };
                for key in 0..count {
                    marf.insert(
                        &format!("patch-key-{key}"),
                        MARFValue::from_value(&format!("{block}-{key}")),
                    )
                    .unwrap();
                }
                marf.commit().unwrap();
            }
            assert_eq!(
                cached.get_root_hash_at(&tip).unwrap(),
                plain.get_root_hash_at(&tip).unwrap()
            );
            for key in 0..512 {
                let key = format!("patch-key-{key}");
                assert_eq!(
                    cached.get(&tip, &key).unwrap(),
                    plain.get(&tip, &key).unwrap()
                );
                assert_eq!(
                    cached.get(&tip, &key).unwrap(),
                    plain.get(&tip, &key).unwrap()
                );
                let size = cached.borrow_storage_backend().resolved_patch_cache_len();
                assert!(size <= 4);
                saw_nonroot_cache |= size > 0;
            }
            let key = "patch-key-3";
            let (cached_value, cached_proof) = cached.get_with_proof(&tip, key).unwrap().unwrap();
            let (plain_value, plain_proof) = plain.get_with_proof(&tip, key).unwrap().unwrap();
            assert_eq!(cached_value, plain_value);
            assert_eq!(
                cached_proof.serialize_to_vec(),
                plain_proof.serialize_to_vec()
            );
            if block > 1 {
                assert_eq!(
                    cached.get(&parent, key).unwrap(),
                    plain.get(&parent, key).unwrap()
                );
            }
            parent = tip;
        }
        assert!(
            saw_nonroot_cache,
            "fixture must exercise non-root patch caching"
        );
        {
            let mut reopened = cached.reopen_connection().unwrap();
            assert_eq!(
                reopened.get(&parent, "patch-key-3").unwrap(),
                plain.get(&parent, "patch-key-3").unwrap()
            );
        }
        // A reader opened before warming observes admissions from a later temporary view.
        let mut sibling = cached.reopen_readonly().unwrap();
        cached.borrow_storage_backend().clear_resolved_patch_cache();
        assert_eq!(
            sibling.borrow_storage_backend().resolved_patch_cache_len(),
            0
        );
        {
            let mut reopened = cached.reopen_connection().unwrap();
            for key in 0..512 {
                let key = format!("patch-key-{key}");
                assert_eq!(
                    reopened.get(&parent, &key).unwrap(),
                    plain.get(&parent, &key).unwrap()
                );
            }
        }
        let warmed = cached.borrow_storage_backend().resolved_patch_cache_len();
        assert!(warmed > 0 && warmed <= 4);
        assert_eq!(
            sibling.borrow_storage_backend().resolved_patch_cache_len(),
            warmed
        );
        // Explicit and implicit SQL rollback invalidate every related handle.
        cached.borrow_storage_transaction().rollback();
        assert_eq!(
            sibling.borrow_storage_backend().resolved_patch_cache_len(),
            0
        );
        for key in 0..512 {
            sibling.get(&parent, &format!("patch-key-{key}")).unwrap();
        }
        assert!(cached.borrow_storage_backend().resolved_patch_cache_len() > 0);
        {
            let _transaction = cached.borrow_storage_transaction();
        }
        assert_eq!(
            sibling.borrow_storage_backend().resolved_patch_cache_len(),
            0
        );
        // A fresh open of the same file has an independent cache.
        let mut fresh_opts = opts.clone();
        fresh_opts.resolved_patch_cache_capacity = 4;
        let fresh = MARF::<StacksBlockId>::from_storage(
            TrieFileStorage::open(cached_path.to_str().unwrap(), fresh_opts).unwrap(),
        );
        for key in 0..512 {
            sibling.get(&parent, &format!("patch-key-{key}")).unwrap();
        }
        let mut fresh = fresh;
        assert_eq!(fresh.borrow_storage_backend().resolved_patch_cache_len(), 0);
        cached.begin(&parent, &StacksBlockId([99; 32])).unwrap();
        cached
            .insert("patch-key-3", MARFValue::from_value("discarded"))
            .unwrap();
        cached.drop_current();
        assert_eq!(
            cached.borrow_storage_backend().resolved_patch_cache_len(),
            0
        );
        assert_eq!(
            cached.get(&parent, "patch-key-3").unwrap(),
            plain.get(&parent, "patch-key-3").unwrap()
        );
    }
}

/// Shared views retain values after eviction and reject loads racing with invalidation.
#[test]
fn shared_lru_lifetimes_and_invalidation() {
    let cache = SharedLru::new(2);
    let sibling = cache.clone();
    let (generation, miss) = cache.get(&1);
    assert!(miss.is_none());
    cache.put_if_current(generation, 1, Arc::new(11));
    let retained = sibling.get(&1).1.unwrap();
    cache.put_if_current(generation, 2, Arc::new(22));
    sibling.get(&1);
    sibling.put_if_current(generation, 3, Arc::new(33));
    assert!(cache.get(&2).1.is_none());
    assert_eq!(cache.len(), 2);
    sibling.clear();
    assert_eq!(*retained, 11);
    assert_eq!(cache.len(), 0);
    cache.put_if_current(generation, 4, Arc::new(44));
    assert_eq!(
        cache.len(),
        0,
        "load from before invalidation must not repopulate"
    );
    let (generation, _) = cache.get(&4);
    cache.put_if_current(generation, 4, Arc::new(44));
    cache.rollback_guard().succeed();
    assert_eq!(sibling.len(), 1);
    drop(cache.rollback_guard());
    assert_eq!(sibling.len(), 0);
    let disabled = SharedLru::new(0);
    disabled.put_if_current(0, 1, Arc::new(1));
    assert!(disabled.get(&1).1.is_none());
}

/// Concurrent readers/admissions share a bounded cache and return intact values.
#[test]
fn shared_lru_concurrent_access() {
    let cache = SharedLru::new(64);
    let start = Arc::new(Barrier::new(4));
    let workers: Vec<_> = (0..4)
        .map(|worker| {
            let cache = cache.clone();
            let start = Arc::clone(&start);
            thread::spawn(move || {
                start.wait();
                for n in 0..2000 {
                    let key = (n + worker * 17) % 97;
                    let (generation, value) = cache.get(&key);
                    if let Some(value) = value {
                        assert_eq!(*value, key * 3);
                    } else {
                        cache.put_if_current(generation, key, Arc::new(key * 3));
                    }
                    if n % 101 == 0 {
                        cache.clear();
                    }
                }
            })
        })
        .collect();
    for worker in workers {
        worker.join().unwrap();
    }
    assert!(cache.len() <= 64);
}
