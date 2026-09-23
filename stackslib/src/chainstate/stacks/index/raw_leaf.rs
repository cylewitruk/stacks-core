//! Compact raw leaves preserve the complete logical value with zero-tail width modes.

use std::io::Write;

use super::{Error, MARFValue, TrieLeaf};

/// Encoded value lengths selected by the two high bits of the path byte.
const WIDTHS: [usize; 4] = [0, 4, 32, 40];

/// Smallest exact representation of the value's nonzero prefix.
fn value_mode(value: &MARFValue) -> usize {
    WIDTHS
        .iter()
        .position(|width| value.0[*width..].iter().all(|byte| *byte == 0))
        .expect("the full-width mode accepts every value")
}

/// Number of physical value bytes, excluding path and marker.
pub fn value_width(value: &MARFValue) -> usize {
    WIDTHS[value_mode(value)]
}

/// Complete payload length derived from its packed path-length/mode byte.
pub fn payload_len(header: u8) -> Result<usize, Error> {
    let path_len = usize::from(header & 0x3f);
    if path_len > 32 {
        return Err(Error::CorruptionError(
            "Compact raw leaf path exceeds32bytes".into(),
        ));
    }
    Ok(1 + path_len + WIDTHS[usize::from(header >> 6)])
}

/// Borrow the path without reconstructing the value.
pub fn path(payload: &[u8]) -> Result<&[u8], Error> {
    let header = *payload
        .first()
        .ok_or_else(|| Error::CorruptionError("Truncated compact raw leaf".into()))?;
    payload_len(header)?;
    payload
        .get(1..1 + usize::from(header & 0x3f))
        .ok_or_else(|| Error::CorruptionError("Truncated compact raw leaf path".into()))
}

/// Encode the packed path byte, exact path, and shortest zero-extended value prefix.
pub fn write<W: Write>(leaf: &TrieLeaf, writer: &mut W) -> Result<(), Error> {
    let value = leaf.value()?;
    let mode = value_mode(value);
    let header = leaf.path.len() as u8 | ((mode as u8) << 6);
    writer.write_all(&[header])?;
    writer.write_all(&leaf.path)?;
    writer.write_all(&value.0[..WIDTHS[mode]])?;
    Ok(())
}

/// Decode a compact value into fixed-size leaf storage without a heap allocation.
pub fn load(leaf: &mut TrieLeaf, payload: &[u8]) -> Result<usize, Error> {
    let path = path(payload)?;
    let consumed = payload_len(payload[0])?;
    let bytes = payload
        .get(1 + path.len()..consumed)
        .ok_or_else(|| Error::CorruptionError("Truncated compact raw leaf value".into()))?;
    leaf.path.set_from_slice(path).ok_or(Error::OverflowError)?;
    let mut value = [0; 40];
    value[..bytes.len()].copy_from_slice(bytes);
    leaf.data = Some(MARFValue(value));
    leaf.extent = None;
    leaf.inline = None;
    Ok(consumed)
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Seek};

    use super::*;
    use crate::chainstate::stacks::index::bits;
    use crate::chainstate::stacks::index::node::{TrieNodeID, TrieNodeType};
    use crate::chainstate::stacks::index::record::{NodeRecordFormat, RecordContext};

    /// Every path length and value width preserves all forty bytes and the logical hash.
    #[test]
    fn raw_widths_roundtrip_and_preserve_commitments() {
        let format = NodeRecordFormat::TypeFirstV2;
        for path_len in 0..=32 {
            for last_nonzero in 0..=40 {
                let mut value = [0; 40];
                for (i, byte) in value[..last_nonzero].iter_mut().enumerate() {
                    *byte = (i + 1) as u8;
                }
                let leaf = TrieLeaf::from_value(&vec![17; path_len], MARFValue(value));
                let hash = bits::get_leaf_hash(&leaf);
                let node = TrieNodeType::Leaf(leaf);
                let mut bytes = Vec::new();
                format.write_node(&mut bytes, &node, hash, true).unwrap();
                assert_eq!(bytes.len(), 2 + path_len + value_width(&MARFValue(value)));
                assert_eq!(bytes.len(), format.node_len(&node, true));
                let record = format.parse(&bytes).unwrap();
                assert_eq!(record.logical_type(), TrieNodeID::Leaf);
                assert!(record.hash.is_none());
                let (decoded, used) = record.decode_node(TrieNodeID::Leaf as u8).unwrap();
                assert_eq!(used, bytes.len());
                assert_eq!(decoded, node);
                let context = RecordContext {
                    format,
                    value_resolver: None,
                };
                assert_eq!(context.hash(record).unwrap(), hash);
                let mut input = Cursor::new(bytes.clone());
                assert_eq!(
                    context.read_probe(&mut input).unwrap(),
                    (TrieNodeID::Leaf, hash)
                );
                assert_eq!(input.stream_position().unwrap(), bytes.len() as u64);
                assert!(NodeRecordFormat::TypeFirstV1.parse(&bytes).is_err());
                for end in 0..bytes.len() {
                    assert!(format
                        .parse(&bytes[..end])
                        .and_then(|r| r.decode_node(TrieNodeID::Leaf as u8))
                        .is_err());
                }
            }
        }
    }

    /// Wider legal width modes still restore the same value; invalid path lengths fail.
    #[test]
    fn raw_width_modes_accept_zero_tails_and_reject_invalid_paths() {
        for mode in 0..4 {
            for length in 33..64 {
                assert!(payload_len((mode << 6) | length).is_err());
            }
            let mut bytes = vec![mode << 6];
            bytes.resize(1 + WIDTHS[mode as usize], 0);
            let mut leaf = TrieLeaf::from_value(&[], MARFValue([1; 40]));
            assert_eq!(load(&mut leaf, &bytes).unwrap(), bytes.len());
            assert_eq!(leaf.value().unwrap(), &MARFValue([0; 40]));
        }
    }
}
