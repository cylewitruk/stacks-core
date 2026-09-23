//! Compact, fork-specific ancestry links for the optional direct-hash sidecar.

/// Byte width of the ancestry extension to a direct-hash record.
pub const ENCODED_SIZE: usize = 16;
/// Bound malformed traversals independently of database or chain size.
const MAX_STEPS: usize = 1024;
/// Marker distinguishing a known genesis from missing ancestry metadata.
const VALID: u32 = 1;

/// An immutable block's height and two older local IDs on its own fork.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Ancestry {
    /// Height in the MARF domain, independent of the local row ID.
    pub height: u32,
    /// Immediate parent's local ID; zero only at genesis.
    pub parent: u32,
    /// Ancestor at `height & (height - 1)`; zero only at genesis.
    pub skip: u32,
}

impl Ancestry {
    /// Describe a genesis trie without a parent.
    pub const fn genesis() -> Self {
        Self {
            height: 0,
            parent: 0,
            skip: 0,
        }
    }

    /// Decode a known record, rejecting unknown flags and impossible local links.
    pub fn decode(id: u32, bytes: &[u8]) -> Option<Self> {
        let bytes: &[u8; ENCODED_SIZE] = bytes.try_into().ok()?;
        let word = |at| u32::from_le_bytes(bytes[at..at + 4].try_into().expect("fixed word"));
        if id == 0 || word(12) != VALID {
            return None;
        }
        let record = Self {
            height: word(0),
            parent: word(4),
            skip: word(8),
        };
        if record.height == 0 {
            return (record.parent == 0 && record.skip == 0).then_some(record);
        }
        (record.parent > 0 && record.parent < id && record.skip > 0 && record.skip <= record.parent)
            .then_some(record)
    }

    /// Serialize four little-endian words without alignment or padding requirements.
    pub fn encode(self) -> [u8; ENCODED_SIZE] {
        let mut bytes = [0; ENCODED_SIZE];
        for (chunk, value) in
            bytes
                .chunks_exact_mut(4)
                .zip([self.height, self.parent, self.skip, VALID])
        {
            chunk.copy_from_slice(&value.to_le_bytes());
        }
        bytes
    }

    /// Derive links from an older parent whose complete ancestry is admitted.
    pub fn child(id: u32, parent: u32, mut read: impl FnMut(u32) -> Option<Self>) -> Option<Self> {
        if parent == 0 || parent >= id {
            return None;
        }
        let height = read(parent)?.height.checked_add(1)?;
        let skip = ancestor(parent, skip_height(height), &mut read)?;
        Some(Self {
            height,
            parent,
            skip,
        })
    }
}

/// Height reached by clearing the lowest set bit; every non-genesis link decreases height.
fn skip_height(height: u32) -> u32 {
    height & height.saturating_sub(1)
}

/// Resolve a height on the starting block's fork, without a global height-to-ID assumption.
/// Missing or malformed metadata requests authoritative MARF fallback.
pub fn ancestor(
    mut id: u32,
    target: u32,
    mut read: impl FnMut(u32) -> Option<Ancestry>,
) -> Option<u32> {
    let mut record = read(id)?;
    if target > record.height {
        return None;
    }
    for _ in 0..MAX_STEPS {
        if record.height == target {
            return Some(id);
        }
        let skip_height = skip_height(record.height);
        let (next, expected) = if skip_height >= target {
            (record.skip, skip_height)
        } else {
            (record.parent, record.height.checked_sub(1)?)
        };
        if next == 0 || next >= id {
            return None;
        }
        let next_record = read(next)?;
        if next_record.height != expected {
            return None;
        }
        id = next;
        record = next_record;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Record IDs can have holes, while heights remain relative to each parent chain.
    #[test]
    fn forked_height_queries_match_parent_walks() {
        let mut records = vec![None, Some(Ancestry::genesis())];
        let mut rng = 7u64;
        for id in 2..12_000u32 {
            if id % 7 == 0 {
                records.push(None);
                continue;
            }
            rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1);
            let mut parent = if id % 11 == 0 {
                1 + (rng as u32 % (id - 1))
            } else {
                id - 1
            };
            while records[parent as usize].is_none() {
                parent -= 1;
            }
            let item = Ancestry::child(id, parent, |i| records.get(i as usize).copied().flatten())
                .unwrap();
            assert_eq!(Ancestry::decode(id, &item.encode()), Some(item));
            records.push(Some(item));
            for height in [
                0,
                item.height / 2,
                item.height.saturating_sub(1),
                item.height,
            ] {
                let mut expected = id;
                while records[expected as usize].unwrap().height > height {
                    expected = records[expected as usize].unwrap().parent;
                }
                assert_eq!(
                    ancestor(id, height, |i| records.get(i as usize).copied().flatten()),
                    Some(expected)
                );
            }
            assert_eq!(
                ancestor(id, item.height + 1, |i| records
                    .get(i as usize)
                    .copied()
                    .flatten()),
                None
            );
        }
    }

    /// Virtual sequential records exercise high heights without allocating a large index.
    #[test]
    fn high_heights_have_bounded_lookup_work() {
        let read = |id: u32| {
            (id > 0).then(|| {
                let height = id - 1;
                if height == 0 {
                    Ancestry::genesis()
                } else {
                    Ancestry {
                        height,
                        parent: id - 1,
                        skip: skip_height(height) + 1,
                    }
                }
            })
        };
        let mut worst = 0;
        for height in [1, 2, 3, 4096, 8_733_721, u32::MAX - 1] {
            for bit in 0..32 {
                let target = height.saturating_sub(1u32 << bit);
                let mut reads = 0;
                assert_eq!(
                    ancestor(height + 1, target, |id| {
                        reads += 1;
                        read(id)
                    }),
                    Some(target + 1)
                );
                worst = worst.max(reads);
            }
        }
        assert!(worst < MAX_STEPS);
    }

    /// Unpublished slots, unknown flags, cycles and inconsistent heights request fallback.
    #[test]
    fn invalid_or_missing_records_fail_closed() {
        assert_eq!(Ancestry::decode(1, &[0; ENCODED_SIZE]), None);
        let genesis = Ancestry::genesis();
        assert_eq!(Ancestry::decode(1, &genesis.encode()), Some(genesis));
        let mut unknown = genesis.encode();
        unknown[12] = 2;
        assert_eq!(Ancestry::decode(1, &unknown), None);
        let cyclic = Ancestry {
            height: 1,
            parent: 2,
            skip: 2,
        };
        assert_eq!(Ancestry::decode(2, &cyclic.encode()), None);
        assert_eq!(ancestor(2, 0, |_| Some(cyclic)), None);
        let bad_height = Ancestry {
            height: 5,
            parent: 1,
            skip: 1,
        };
        assert_eq!(
            ancestor(2, 0, |id| Some(if id == 2 { bad_height } else { genesis })),
            None
        );
        assert_eq!(Ancestry::child(2, 1, |_| None), None);
        assert_eq!(
            Ancestry::child(2, 1, |_| Some(Ancestry {
                height: u32::MAX,
                parent: 0,
                skip: 0
            })),
            None
        );
    }
}
