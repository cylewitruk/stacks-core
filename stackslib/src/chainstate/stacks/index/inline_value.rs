//! Retained byte owners for codec-independent inline leaf payloads.

use std::io::{self, Write};
use std::ops::Range;
use std::sync::Arc;

use super::{Error, FileMapping, ValueExtent};

/// Bytes used by the independent record and descriptor lengths.
pub const LENGTH_BYTES: usize = 2;
/// Writer threshold that does not exceed the existing physical extent locator.
pub const INLINE_BYTES: usize = ValueExtent::ENCODED_SIZE - LENGTH_BYTES;
/// Largest payload accepted by the two one-byte length fields.
pub const MAX_BYTES: usize = 2 * u8::MAX as usize;

/// Immutable bytes retained independently of the MARF handle and its read scratch.
#[derive(Debug)]
pub struct InlineValueBytes {
    /// Checked mapping range or an owned pending/fallback value.
    backing: InlineBacking,
}

/// Storage for an immutable inline record and its reconstruction descriptor.
#[derive(Debug)]
enum InlineBacking {
    /// A published immutable region of a retained trie mapping.
    Mapped {
        /// Shared stable reservation or conventional mapping.
        mapping: FileMapping,
        /// Exact value bytes, excluding physical leaf framing.
        range: Range<usize>,
    },
    /// Bytes decoded from pending writes, SQLite or an unmapped EOF region.
    Owned(Box<[u8]>),
}

impl AsRef<[u8]> for InlineValueBytes {
    fn as_ref(&self) -> &[u8] {
        match &self.backing {
            InlineBacking::Mapped { mapping, range } => &mapping[range.clone()],
            InlineBacking::Owned(bytes) => bytes,
        }
    }
}

/// Codec-independent record and descriptor retained by an inline leaf.
#[derive(Clone, Debug)]
pub struct InlineValue {
    /// Owner shared by leaf caches and projected VM values.
    owner: Arc<InlineValueBytes>,
    /// Split between the encoded record and its reconstruction descriptor.
    record_len: u8,
}

impl PartialEq for InlineValue {
    fn eq(&self, other: &Self) -> bool {
        self.record_len == other.record_len
            && (Arc::ptr_eq(&self.owner, &other.owner)
                || self.owner.as_ref().as_ref() == other.owner.as_ref().as_ref())
    }
}

impl Eq for InlineValue {}

impl InlineValue {
    /// Whether a test value retains trie mapping bytes rather than a fallback copy.
    #[cfg(test)]
    pub fn is_mapped(&self) -> bool {
        matches!(self.owner.backing, InlineBacking::Mapped { .. })
    }

    /// Retain pending or fallback bytes after checking physical length bounds.
    pub fn from_parts(record: &[u8], descriptor: &[u8]) -> Result<Self, Error> {
        let record_len = u8::try_from(record.len()).map_err(|_| Error::OverflowError)?;
        u8::try_from(descriptor.len()).map_err(|_| Error::OverflowError)?;
        let mut bytes = Vec::with_capacity(record.len() + descriptor.len());
        bytes.extend_from_slice(record);
        bytes.extend_from_slice(descriptor);
        Ok(Self {
            owner: Arc::new(InlineValueBytes {
                backing: InlineBacking::Owned(bytes.into_boxed_slice()),
            }),
            record_len,
        })
    }

    /// Retain checked published mapping bytes without copying or decoding their payload.
    pub fn from_mapping(
        mapping: FileMapping,
        range: Range<usize>,
        record_len: u8,
    ) -> Result<Self, Error> {
        let size = range
            .end
            .checked_sub(range.start)
            .ok_or(Error::OverflowError)?;
        let descriptor = size
            .checked_sub(usize::from(record_len))
            .ok_or(Error::OverflowError)?;
        if range.end > mapping.len() || descriptor > usize::from(u8::MAX) {
            return Err(Error::CorruptionError(
                "Invalid mapped inline value range".into(),
            ));
        }
        Ok(Self {
            owner: Arc::new(InlineValueBytes {
                backing: InlineBacking::Mapped { mapping, range },
            }),
            record_len,
        })
    }

    /// Borrow the encoded value record without consulting any external value store.
    pub fn record(&self) -> &[u8] {
        &self.owner.as_ref().as_ref()[..usize::from(self.record_len)]
    }

    /// Borrow the exact canonical reconstruction descriptor.
    pub fn descriptor(&self) -> &[u8] {
        &self.owner.as_ref().as_ref()[usize::from(self.record_len)..]
    }

    /// Retain the shared byte owner for a projected VM value.
    pub fn owner(&self) -> Arc<InlineValueBytes> {
        Arc::clone(&self.owner)
    }

    /// Encoded record range within the retained byte owner.
    pub fn record_range(&self) -> Range<usize> {
        0..usize::from(self.record_len)
    }

    /// Reconstruction descriptor range within the retained byte owner.
    pub fn descriptor_range(&self) -> Range<usize> {
        usize::from(self.record_len)..self.owner.as_ref().as_ref().len()
    }

    /// Physical length including both one-byte lengths, excluding path and node marker.
    pub fn encoded_len(&self) -> usize {
        LENGTH_BYTES + self.owner.as_ref().as_ref().len()
    }

    /// Whether this payload fits the standard writer's extent-locator break-even rule.
    pub fn fits_inline(record_len: usize, descriptor_len: usize) -> bool {
        record_len
            .checked_add(descriptor_len)
            .is_some_and(|len| len <= INLINE_BYTES)
    }

    /// Write length framing followed by immutable record and descriptor bytes.
    pub fn write_to<W: Write>(&self, writer: &mut W) -> io::Result<()> {
        writer.write_all(&[self.record_len, self.descriptor().len() as u8])?;
        writer.write_all(self.owner.as_ref().as_ref())
    }

    /// Decode one framed value into owned fallback storage, permitting a trailing record.
    pub fn decode(bytes: &[u8]) -> Result<(Self, usize), Error> {
        let (record, descriptor, used) = Self::ranges(bytes)?;
        Ok((Self::from_parts(&bytes[record], &bytes[descriptor])?, used))
    }

    /// Check framing and return record/descriptor ranges without inspecting their contents.
    pub fn ranges(bytes: &[u8]) -> Result<(Range<usize>, Range<usize>, usize), Error> {
        let lengths = bytes
            .get(..LENGTH_BYTES)
            .ok_or_else(|| Error::CorruptionError("Truncated inline value lengths".into()))?;
        let record_end = LENGTH_BYTES + usize::from(lengths[0]);
        let end = record_end + usize::from(lengths[1]);
        if end > bytes.len() {
            return Err(Error::CorruptionError(
                "Truncated inline value payload".into(),
            ));
        }
        Ok((LENGTH_BYTES..record_end, record_end..end, end))
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Seek, SeekFrom, Write};

    use super::*;

    /// Reader bounds remain independent of the compact writer threshold.
    #[test]
    fn inline_framing_preserves_boundaries_and_rejects_truncation() {
        for (record, descriptor) in [(0, 0), (2, 28), (30, 0), (31, 0), (255, 255)] {
            let value = InlineValue::from_parts(&vec![7; record], &vec![9; descriptor]).unwrap();
            let mut encoded = Vec::new();
            value.write_to(&mut encoded).unwrap();
            assert_eq!(
                InlineValue::decode(&encoded).unwrap(),
                (value.clone(), encoded.len())
            );
            assert_eq!(
                InlineValue::fits_inline(record, descriptor),
                record + descriptor <= 30
            );
            for end in 0..encoded.len() {
                assert!(InlineValue::decode(&encoded[..end]).is_err());
            }
            let length = encoded.len();
            encoded.extend_from_slice(&[17; 32]);
            assert_eq!(InlineValue::decode(&encoded).unwrap(), (value, length));
        }
        assert!(!InlineValue::fits_inline(usize::MAX, 1));
        assert!(InlineValue::from_parts(&[0; 256], &[]).is_err());
        assert!(InlineValue::from_parts(&[], &[0; 256]).is_err());
    }

    /// Multiple values retain their exact mapping ranges across handle drop and extension.
    #[test]
    fn mapped_inline_owners_survive_read_handles_and_append() {
        let mut file = tempfile::tempfile().unwrap();
        file.set_len(128 * 1024).unwrap();
        file.seek(SeekFrom::Start(24)).unwrap();
        file.write_all(b"first-shape-second-kind").unwrap();
        file.flush().unwrap();
        // SAFETY: Published bytes are immutable; later operations only extend the file.
        let mut mapping = unsafe { FileMapping::map(&file).unwrap() };
        let address = mapping[24..].as_ptr();
        let first = InlineValue::from_mapping(mapping.clone(), 24..35, 5).unwrap();
        let second = InlineValue::from_mapping(mapping.clone(), 36..47, 6).unwrap();
        assert_eq!(first.record().as_ptr(), address);
        assert_eq!(first.record(), b"first");
        assert_eq!(first.descriptor(), b"-shape");
        assert_eq!(second.record(), b"second");
        let retained = first.owner();
        drop(first);
        file.set_len(256 * 1024).unwrap();
        // SAFETY: Same append-only file, without modifying the retained prefix.
        unsafe { mapping.refresh(&file).unwrap() };
        drop(mapping);
        drop(file);
        assert_eq!(retained.as_ref().as_ref().as_ptr(), address);
        assert_eq!(retained.as_ref().as_ref(), b"first-shape");
        assert_eq!(second.descriptor(), b"-kind");
    }

    /// The mapped constructor cannot expose unbacked bytes or an invalid descriptor split.
    #[test]
    fn mapped_inline_ranges_are_bounded() {
        let file = tempfile::tempfile().unwrap();
        file.set_len(128 * 1024).unwrap();
        // SAFETY: This test never changes the file after mapping.
        let mapping = unsafe { FileMapping::map(&file).unwrap() };
        assert!(InlineValue::from_mapping(mapping.clone(), 10..9, 0).is_err());
        assert!(InlineValue::from_mapping(mapping.clone(), 10..11, 2).is_err());
        assert!(InlineValue::from_mapping(mapping.clone(), 0..256, 0).is_err());
        assert!(InlineValue::from_mapping(mapping.clone(), 0..mapping.len() + 1, 1).is_err());
        assert!(InlineValue::from_mapping(mapping, 0..510, 255).is_ok());
    }
}
