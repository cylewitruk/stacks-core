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

//! Read-only sampled leaf, proof and ancestry qualification of a legacy/canonical pair.

use std::collections::HashSet;
use std::env;
use std::fs::File;
use std::io::Cursor;
use std::path::Path;
use std::sync::{Arc, Mutex};

use blockstack_lib::chainstate::stacks::index::direct_hash_index::DirectHashIndex;
use blockstack_lib::chainstate::stacks::index::marf::{
    BLOCK_HASH_TO_HEIGHT_MAPPING_KEY, BLOCK_HEIGHT_TO_HASH_MAPPING_KEY, MARF, MARFOpenOpts,
    MarfConnection, OWN_BLOCK_HEIGHT_KEY,
};
use blockstack_lib::chainstate::stacks::index::node::{TrieNodeType, is_backptr, logical_node_id};
use blockstack_lib::chainstate::stacks::index::record::{NodeRecordFormat, RecordContext};
use blockstack_lib::chainstate::stacks::index::scratch::MarfReadState;
use blockstack_lib::chainstate::stacks::index::storage::TrieFileStorage;
use blockstack_lib::chainstate::stacks::index::{MARFValue, ReadTrieItemKind, bits};
use blockstack_lib::clarity_vm::database::value_extents::ValueBackend;
use blockstack_lib::util_lib::db::sqlite_readonly_uri;
use memmap2::Mmap;
use rusqlite::{Connection, OpenFlags};
use stacks_common::types::chainstate::StacksBlockId;
use stacks_storage_migrate::Result;

/// An offline SQL snapshot, optional external mapping and registered value owner.
struct Input {
    /// Immutable SQL connection; no journal creation or checkpoints.
    db: Connection,
    /// Mapped source bytes, retained for the complete audit.
    blobs: Option<Mmap>,
    /// Explicit physical layout and optional Clarity commitment resolver.
    context: RecordContext,
    /// Ordinary read-only MARF APIs used for proof qualification.
    marf: MARF<StacksBlockId>,
}

impl Input {
    /// Open without modifying either source, including legacy internal-blob inputs.
    fn open(path: &Path) -> Result<Self> {
        let db = Connection::open_with_flags(
            sqlite_readonly_uri(path, true)?,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
        )?;
        let format = NodeRecordFormat::from_database(&db)?;
        let file = path.with_file_name(format!(
            "{}.blobs",
            path.file_name()
                .ok_or("missing filename")?
                .to_str()
                .ok_or("filename encoding")?
        ));
        let blobs = if file.exists() && file.metadata()?.len() > 0 {
            // SAFETY: both inputs must remain offline and immutable until this audit finishes.
            Some(unsafe { Mmap::map(&File::open(&file)?)? })
        } else {
            None
        };
        let mut opts = MARFOpenOpts::default().with_mmap(true);
        opts.external_blobs = file.exists();
        let mut marf = MARF::from_storage(TrieFileStorage::open_readonly(
            path.to_str().ok_or("path encoding")?,
            opts,
        )?);
        let mut context = RecordContext {
            format,
            value_resolver: None,
        };
        if let Some(values) = ValueBackend::open_registered(&db, path)? {
            let resolver = Arc::new(Mutex::new(values));
            marf.set_value_resolver(resolver.clone());
            context.value_resolver = Some(resolver);
        }
        Ok(Self {
            db,
            blobs,
            context,
            marf,
        })
    }

    /// Compare logical local leaves independently of their physical representation.
    fn leaves(&self, id: u32) -> Result<Vec<(Vec<u8>, MARFValue)>> {
        let external: bool = self.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('marf_data') WHERE name='external_offset')",
            [], |row| row.get(0),
        )?;
        let location = if external {
            "external_offset,external_length"
        } else {
            "0,0"
        };
        let (inline, offset, length): (Vec<u8>, u64, u64) = self.db.query_row(
            &format!("SELECT data,{location} FROM marf_data WHERE block_id=?1"),
            [id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        let bytes = if inline.is_empty() {
            let start = usize::try_from(offset)?;
            let end = usize::try_from(offset.checked_add(length).ok_or("location overflow")?)?;
            self.blobs
                .as_ref()
                .and_then(|map| map.get(start..end))
                .ok_or("invalid location")?
        } else {
            &inline
        };
        self.context.format.validate_trie_header(bytes)?;
        let mut leaves = Vec::new();
        let mut pending = vec![36usize];
        let mut visited = HashSet::new();
        let mut input = Cursor::new(bytes);
        let mut scratch = MarfReadState::new();
        while let Some(offset) = pending.pop() {
            if !visited.insert(offset) {
                continue;
            }
            let record = self
                .context
                .format
                .parse(bytes.get(offset..).ok_or("node bounds")?)?;
            input.set_position(offset as u64);
            let item = bits::read_trie_item_at_head_ref_format(
                &mut input,
                logical_node_id(record.marker),
                self.context.format,
                &mut scratch,
            )?;
            let pointers = match item.kind {
                ReadTrieItemKind::Node(node) => match node.into_owned_node()?.0 {
                    TrieNodeType::Leaf(mut leaf) => {
                        self.context.resolve_leaf(&mut leaf)?;
                        leaves.push((leaf.path.to_vec(), leaf.value()?.clone()));
                        Vec::new()
                    }
                    branch => branch.ptrs().to_vec(),
                },
                ReadTrieItemKind::Patch(patch) => std::iter::once(patch.ptr)
                    .chain(patch.ptr_diff.iter().copied())
                    .collect(),
            };
            for ptr in pointers.iter().rev() {
                if !ptr.is_empty() && !is_backptr(ptr.id()) {
                    pending.push(usize::try_from(ptr.ptr())?);
                }
            }
        }
        Ok(leaves)
    }
}

/// Sample uniformly across source IDs and verify local values, ordinary proofs and direct ancestry.
fn main() -> Result<()> {
    let args: Vec<_> = env::args().skip(1).collect();
    if args.len() != 2 {
        return Err("usage: verify_pair LEGACY_SQLITE CANONICAL_SQLITE".into());
    }
    let mut before = Input::open(Path::new(&args[0]))?;
    let mut after = Input::open(Path::new(&args[1]))?;
    if before.context.format != NodeRecordFormat::Legacy
        || after.context.format != NodeRecordFormat::Optimized
    {
        return Err("expected legacy/canonical pair".into());
    }
    let max: u32 = before
        .db
        .query_row("SELECT max(block_id) FROM marf_data", [], |row| row.get(0))?;
    let mut ids = Vec::new();
    for part in 0..64u64 {
        let target = 1 + u64::from(max.saturating_sub(1)) * part / 63;
        let id: u32 = before.db.query_row(
            "SELECT min(block_id) FROM marf_data WHERE block_id>=?1",
            [target],
            |row| row.get(0),
        )?;
        if ids.last() != Some(&id) {
            ids.push(id);
        }
    }
    let mut direct =
        DirectHashIndex::open(&after.db, Path::new(&args[1]))?.ok_or("missing direct index")?;
    after.db.execute_batch("BEGIN")?;
    let _guard = direct.begin(&after.db)?;
    let mut leaves = 0;
    let mut proofs = 0;
    let mut ancestry = 0;
    for id in &ids {
        let a = before.leaves(*id)?;
        let b = after.leaves(*id)?;
        if a != b {
            return Err(format!("local leaf commitment/path mismatch at {id}").into());
        }
        leaves += a.len();
        let block: StacksBlockId = before.db.query_row(
            "SELECT block_hash FROM marf_data WHERE block_id=?1",
            [id],
            |row| row.get(0),
        )?;
        if before.marf.get_root_hash_at(&block)? != after.marf.get_root_hash_at(&block)? {
            return Err(format!("root mismatch at {id}").into());
        }
        let height = u32::from(
            before
                .marf
                .get(&block, OWN_BLOCK_HEIGHT_KEY)?
                .ok_or("missing own height")?,
        );
        let mut keys = vec![OWN_BLOCK_HEIGHT_KEY.to_owned()];
        for target in [0, height / 2, height.saturating_sub(1), height] {
            let expected = if target == height {
                block.clone()
            } else {
                let key = format!("{BLOCK_HEIGHT_TO_HASH_MAPPING_KEY}::{target}");
                let hash =
                    StacksBlockId::from(before.marf.get(&block, &key)?.ok_or("missing mapping")?);
                keys.push(key);
                keys.push(format!("{BLOCK_HASH_TO_HEIGHT_MAPPING_KEY}::{hash}"));
                hash
            };
            if direct.ancestor_hash(&after.db, *id, &block, height, target) != Some(expected) {
                return Err(format!("ancestry mismatch at {id}").into());
            }
            ancestry += 1;
        }
        keys.sort();
        keys.dedup();
        for key in keys {
            let a = before
                .marf
                .get_with_proof(&block, &key)?
                .map(|(value, proof)| (value, proof.to_hex()));
            let b = after
                .marf
                .get_with_proof(&block, &key)?
                .map(|(value, proof)| (value, proof.to_hex()));
            if a.is_none() || a != b {
                return Err(format!("proof mismatch at {id}: {key}").into());
            }
            proofs += 1;
        }
    }
    if leaves == 0 {
        return Err("sample did not exercise any leaves".into());
    }
    println!(
        "{}",
        serde_json::json!({"passed":true,"sampled_tries":ids.len(),"local_leaf_commitments":leaves,"proofs":proofs,"ancestry":ancestry})
    );
    Ok(())
}
