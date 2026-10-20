//! Per-record statistics of every confirmed trie blob in a MARF database,
//! decoded with the rewrite's codec (which reads both implementations' output).

use std::collections::BTreeMap;

use rusqlite::{Connection, OpenFlags};
use stacks_marf_next::codec::{self, Decoded};
use stacks_marf_next::node::{id, Node, Ptr};

pub fn run(db_path: &str) {
    let conn =
        Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY).expect("open");
    let blobs = std::fs::read(format!("{db_path}.blobs")).ok();
    let mut stmt = conn
        .prepare("SELECT block_id, data, external_offset, external_length FROM marf_data WHERE unconfirmed = 0 ORDER BY block_id")
        .expect("prepare");
    let rows: Vec<(u32, Vec<u8>, i64, i64)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .expect("query")
        .map(|r| r.expect("row"))
        .collect();

    let mut counts: BTreeMap<String, (u64, u64)> = BTreeMap::new();
    let mut total = 0u64;
    for (_, data, off, len) in rows {
        let blob: &[u8] = if len > 0 {
            &blobs.as_ref().expect("blobs file")[off as usize..(off + len) as usize]
        } else {
            &data
        };
        total += blob.len() as u64;
        let mut stack: Vec<(u64, u8)> = vec![(36, id::NODE256)];
        while let Some((offset, kind)) = stack.pop() {
            let rec = &blob[offset as usize..];
            let (decoded, used) = codec::decode(&rec[32..], kind).expect("decode");
            let key = match &decoded {
                Decoded::Patch(p) => format!("patch[{}]", p.diff.len().min(9)),
                Decoded::Node(Node::Leaf(_)) => "leaf".to_string(),
                Decoded::Node(Node::Branch(b)) => {
                    format!("{:?}{}", b.kind, if rec[32] & 0x40 != 0 { "c" } else { "" })
                }
            };
            let e = counts.entry(key).or_default();
            e.0 += 1;
            e.1 += 32 + used as u64;
            let children: Vec<Ptr> = match &decoded {
                Decoded::Patch(p) => p.diff.clone(),
                Decoded::Node(Node::Branch(b)) => b.children.clone(),
                _ => vec![],
            };
            for c in children {
                if !c.is_back() {
                    stack.push((c.ptr, c.kind_id()));
                }
            }
        }
    }
    println!("{db_path}: {total} blob bytes");
    for (k, (n, bytes)) in counts {
        println!("  {k:>10}: {n:>8} records {bytes:>10} bytes");
    }
}
