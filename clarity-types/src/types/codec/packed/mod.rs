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

//! Canonical packed representation for Clarity values.
//!
//! This is not Clarity's consensus serialization. Packed value records and their optional
//! value descriptors are independently versioned. Encoding selects each version explicitly;
//! decoding dispatches from the leading version byte and rejects unknown versions without probing
//! another grammar.
//!
//! The version registry and byte-level specifications are linked from the
//! [packed-codec overview][format-spec].
//!
//! [format-spec]: https://github.com/stacks-network/stacks-core/blob/main/clarity-types/src/types/codec/packed/README.md
//!
//! An **expected type** is the caller-supplied [`TypeSignature`] used for typed decoding. A
//! **value descriptor** provides value-derived structure for consensus reconstruction. **Layout**
//! means the physical arrangement of packed bytes; the encoder selects it from the value alone.
//!
//! Two invariants establish canonical byte identity:
//!
//! - Packed bytes depend only on the active [`Value`], never declared bounds or epoch;
//! - [`ValueDescriptor`] is likewise value-derived and supplies structural metadata and independent
//!   framing for exact consensus reconstruction without an expected [`TypeSignature`].

#[cfg(feature = "direct-value-diagnostics")]
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(feature = "direct-value-diagnostics")]
static VALUE_READ_COUNTERS: [AtomicU64; 6] = [const { AtomicU64::new(0) }; 6];

/// Process-wide codec diagnostics: copied bytes, owner validations, validated bytes,
/// owned materializations, their consensus bytes, and shared child projections.
#[cfg(feature = "direct-value-diagnostics")]
pub fn value_read_diagnostics() -> [u64; 6] {
    std::array::from_fn(|index| VALUE_READ_COUNTERS[index].load(Ordering::Relaxed))
}

use std::fmt::Debug;
use std::iter;
use std::ops::Range;
use std::sync::{Arc, OnceLock};
#[cfg(any(test, feature = "testing"))]
use std::{cell::RefCell, panic::Location};

use stacks_common::types::StacksEpochId;

use crate::types::{
    BufferLength, ListTypeData, SequenceSubtype, StringSubtype, StringUTF8Length, TypeSignature,
    Value,
};
#[cfg(any(test, feature = "testing"))]
thread_local! { static SHARED_MATERIALIZATION_LOCATIONS: RefCell<Vec<&'static Location<'static>>> = const { RefCell::new(Vec::new()) }; }
pub use v1::{
    PackedListView, PackedPrincipalView, PackedTupleView, PackedValueKind, PackedValueView,
};

mod composite;
mod serialize;
use composite::CompositeValue;

mod error;
mod shape;
mod v1;

pub use error::{
    ExpectedTypeError, PackedCodecInvariant, PackedRecordError, PackedValueError,
    ReconstructionError, ValueDescriptorError,
};

/// Supported packed value-record wire versions.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
#[repr(u8)]
pub enum PackedValueVersion {
    /// Packed Grammar V1.
    V1 = 1,
}

impl PackedValueVersion {
    /// Return this version's wire discriminator.
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// Return this version's record-header length.
    pub const fn header_len(self) -> usize {
        match self {
            Self::V1 => v1::PACKED_VALUE_HEADER_LEN,
        }
    }

    /// Return this version's maximum packed-body length.
    pub const fn maximum_body_len(self) -> usize {
        match self {
            Self::V1 => v1::BOUND_PACKED_VALUE_BODY_BYTES,
        }
    }

    /// Parse a packed value-record version byte.
    fn from_u8(byte: u8) -> Result<Self, PackedValueError> {
        match byte {
            value if value == Self::V1.as_u8() => Ok(Self::V1),
            version => Err(PackedValueError::UnsupportedPackedValueVersion { version }),
        }
    }
}

/// Supported value descriptor wire versions.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
#[repr(u8)]
pub enum ValueDescriptorVersion {
    /// Value descriptor V1.
    V1 = 1,
}

impl ValueDescriptorVersion {
    /// Return this version's wire discriminator.
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// Return this version's maximum complete descriptor length.
    pub const fn maximum_descriptor_len(self) -> usize {
        match self {
            Self::V1 => v1::BOUND_VALUE_DESCRIPTOR_BYTES,
        }
    }

    /// Parse a value descriptor version byte.
    fn from_u8(byte: u8) -> Result<Self, PackedValueError> {
        match byte {
            value if value == Self::V1.as_u8() => Ok(Self::V1),
            version => Err(PackedValueError::UnsupportedValueDescriptorVersion { version }),
        }
    }
}

/// The encoded bytes and logical length produced by the packed codec.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PackedValue {
    /// Complete versioned packed record.
    bytes: Vec<u8>,
    /// Parsed record version.
    version: PackedValueVersion,
    /// Equivalent consensus-serialization length cached from the record header.
    consensus_byte_len: u32,
}

impl PackedValue {
    /// Measure packed record bytes without producing an encoded payload or descriptor.
    pub fn encoded_byte_len(
        version: PackedValueVersion,
        value: &Value,
    ) -> Result<usize, PackedValueError> {
        match version {
            PackedValueVersion::V1 => v1::encoded_byte_len(value),
        }
    }

    /// Encode one runtime value into its canonical packed representation.
    ///
    /// This operation is independent of declared types and execution epochs. Callers that require
    /// typed admission must perform it before encoding.
    pub fn encode(version: PackedValueVersion, value: &Value) -> Result<Self, PackedValueError> {
        match version {
            PackedValueVersion::V1 => v1::encode(value),
        }
    }

    /// Encode one runtime value after an opaque caller-owned prefix in one allocation.
    ///
    /// The returned buffer contains `prefix` followed by one complete packed record. The prefix is
    /// not interpreted by this codec. `consensus_byte_len` must come from the exact consensus
    /// serialization of `value`, and callers must keep the value and its derived length coupled so
    /// they cannot be mispaired. Trusting that already-proven length avoids repeating a full
    /// consensus-serialization traversal in storage hot paths.
    pub fn encode_with_prefix(
        version: PackedValueVersion,
        value: &Value,
        prefix: &[u8],
        consensus_byte_len: u32,
    ) -> Result<Vec<u8>, PackedValueError> {
        match version {
            PackedValueVersion::V1 => v1::encode_with_prefix(value, prefix, consensus_byte_len),
        }
    }

    /// Transcode one exact self-describing consensus value into canonical packed bytes.
    ///
    /// Historical unsanitized values can contain active data omitted by cached type metadata.
    /// The resulting record preserves that data, but typed decoding under the narrower expected
    /// type is not guaranteed. Call [`Self::transcode_consensus_with_descriptor`] for the
    /// descriptor required for compatibility reconstruction without a caller-supplied type.
    pub fn transcode_consensus(
        version: PackedValueVersion,
        consensus: &[u8],
    ) -> Result<Self, PackedValueError> {
        match version {
            PackedValueVersion::V1 => v1::transcode_consensus(consensus),
        }
    }

    /// Transcode consensus bytes and derive their descriptor-guided reconstruction metadata.
    ///
    /// The returned descriptor allows exact reconstruction when a historical unsanitized value
    /// cannot be decoded directly under its cached type metadata.
    pub fn transcode_consensus_with_descriptor(
        packed_version: PackedValueVersion,
        descriptor_version: ValueDescriptorVersion,
        consensus: &[u8],
    ) -> Result<(Self, ValueDescriptor), PackedValueError> {
        match (packed_version, descriptor_version) {
            (PackedValueVersion::V1, ValueDescriptorVersion::V1) => {
                v1::transcode_consensus_with_descriptor(consensus)
            }
        }
    }

    /// Return the complete packed record bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Consume this record and return its bytes.
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    /// Return the equivalent consensus-serialization length.
    pub fn consensus_byte_len(&self) -> u32 {
        self.consensus_byte_len
    }

    /// Return this record's packed wire version.
    pub const fn version(&self) -> PackedValueVersion {
        self.version
    }

    /// Borrow this owned record through the packed record read API.
    pub fn as_packed_ref(&self) -> PackedValueRef<'_> {
        PackedValueRef {
            bytes: &self.bytes,
            version: self.version,
            consensus_byte_len: self.consensus_byte_len,
        }
    }
}

/// A borrowed view over one packed value record with a validated envelope.
///
/// Parsing validates its version, header, and physical body bound. Typed decoding or audited
/// reconstruction validates the body grammar and canonical encoding.
#[derive(Clone, Copy, Debug)]
pub struct PackedValueRef<'a> {
    /// Complete packed record bytes.
    bytes: &'a [u8],
    /// Parsed record version.
    version: PackedValueVersion,
    /// Equivalent consensus-serialization length read from the record header.
    consensus_byte_len: u32,
}

impl<'a> PackedValueRef<'a> {
    /// Dispatch to the declared version and parse its envelope without decoding the value body.
    pub fn parse(bytes: &'a [u8]) -> Result<Self, PackedValueError> {
        let version =
            PackedValueVersion::from_u8(bytes.first().copied().ok_or(PackedRecordError::Empty)?)?;
        let consensus_byte_len = match version {
            PackedValueVersion::V1 => v1::parse_record(bytes)?,
        };
        Ok(Self {
            bytes,
            version,
            consensus_byte_len,
        })
    }

    /// Return the complete borrowed record bytes.
    pub fn as_bytes(self) -> &'a [u8] {
        self.bytes
    }

    /// Return the equivalent consensus-serialization length from the record header.
    pub const fn consensus_byte_len(self) -> u32 {
        self.consensus_byte_len
    }

    /// Return this record's packed wire version.
    pub const fn version(self) -> PackedValueVersion {
        self.version
    }

    /// Decode and validate this record under an expected type.
    pub fn decode(self, expected: &TypeSignature) -> Result<DecodedPackedValue, PackedValueError> {
        match self.version {
            PackedValueVersion::V1 => v1::decode(self, expected),
        }
    }

    /// Check encoder-produced shape metadata against the VM's declared storage schema.
    /// This checks layout compatibility, not untrusted payload validity or schema admission.
    pub fn matches_storage_schema(
        self,
        descriptor: &[u8],
        expected: &TypeSignature,
    ) -> Result<bool, PackedValueError> {
        match self.version {
            PackedValueVersion::V1 => v1::matches_storage_schema(descriptor, expected),
        }
    }

    /// Reconstruct exact consensus bytes using a value descriptor.
    ///
    /// This checks framing and the declared logical length, but does not prove that the output is a
    /// valid bounded Clarity value. Use [`Self::audit_reconstruction`] for untrusted records.
    pub fn reconstruct_consensus(self, descriptor: &[u8]) -> Result<Vec<u8>, PackedValueError> {
        self.reconstruct_consensus_with_descriptor(ValueDescriptorRef::parse(descriptor)?)
    }

    /// Reconstruct exact consensus bytes using an already parsed value descriptor.
    pub fn reconstruct_consensus_with_descriptor(
        self,
        descriptor: ValueDescriptorRef<'_>,
    ) -> Result<Vec<u8>, PackedValueError> {
        match (self.version, descriptor.version) {
            (PackedValueVersion::V1, ValueDescriptorVersion::V1) => {
                v1::reconstruct_consensus(self, descriptor)
            }
        }
    }

    /// Reconstruct consensus bytes and prove this record and descriptor are canonical.
    pub fn audit_reconstruction(self, descriptor: &[u8]) -> Result<Vec<u8>, PackedValueError> {
        self.audit_reconstruction_with_descriptor(ValueDescriptorRef::parse(descriptor)?)
    }

    /// Audit reconstruction using an already parsed value descriptor.
    pub fn audit_reconstruction_with_descriptor(
        self,
        descriptor: ValueDescriptorRef<'_>,
    ) -> Result<Vec<u8>, PackedValueError> {
        match (self.version, descriptor.version) {
            (PackedValueVersion::V1, ValueDescriptorVersion::V1) => {
                v1::audit_reconstruction(self, descriptor)
            }
        }
    }

    /// Return the packed body after its version-specific envelope.
    fn body(self) -> &'a [u8] {
        &self.bytes[self.version.header_len()..]
    }
}

/// An owned, versioned value descriptor.
///
/// The descriptor supplies structural metadata for reconstructing consensus bytes from
/// [`PackedValue`] without an expected [`TypeSignature`]. Its independent framing may repeat
/// information present in the packed record, such as per-element list counts.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ValueDescriptor {
    /// Complete versioned descriptor bytes.
    bytes: Vec<u8>,
    /// Parsed descriptor version.
    version: ValueDescriptorVersion,
}

impl ValueDescriptor {
    /// Derive canonical reconstruction metadata solely from an active value.
    pub fn from_value(
        version: ValueDescriptorVersion,
        value: &Value,
    ) -> Result<Self, PackedValueError> {
        match version {
            ValueDescriptorVersion::V1 => v1::encode_descriptor(value),
        }
    }

    /// Borrow the complete versioned descriptor.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Consume this descriptor and return its bytes.
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    /// Return this descriptor's wire version.
    pub const fn version(&self) -> ValueDescriptorVersion {
        self.version
    }

    /// Borrow this owned descriptor through the descriptor read API.
    pub fn as_descriptor_ref(&self) -> ValueDescriptorRef<'_> {
        ValueDescriptorRef {
            bytes: &self.bytes,
            version: self.version,
        }
    }

    /// Parse and validate one complete versioned descriptor.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, PackedValueError> {
        let descriptor = ValueDescriptorRef::parse(bytes)?;
        descriptor.validate()?;
        Ok(Self {
            bytes: bytes.to_vec(),
            version: descriptor.version,
        })
    }
}

/// A borrowed view over one versioned value descriptor.
#[derive(Clone, Copy, Debug)]
pub struct ValueDescriptorRef<'a> {
    /// Complete descriptor bytes.
    bytes: &'a [u8],
    /// Parsed descriptor version.
    version: ValueDescriptorVersion,
}

impl<'a> ValueDescriptorRef<'a> {
    /// Parse the versioned descriptor envelope without materializing its recursive shape.
    pub fn parse(bytes: &'a [u8]) -> Result<Self, PackedValueError> {
        let version = ValueDescriptorVersion::from_u8(
            bytes.first().copied().ok_or(ValueDescriptorError::Empty)?,
        )?;
        if bytes.len() > version.maximum_descriptor_len() {
            return Err(ValueDescriptorError::TooLarge {
                actual: bytes.len(),
                maximum: version.maximum_descriptor_len(),
            }
            .into());
        }
        Ok(Self { bytes, version })
    }

    /// Return the complete borrowed descriptor bytes.
    pub const fn as_bytes(self) -> &'a [u8] {
        self.bytes
    }

    /// Return this descriptor's wire version.
    pub const fn version(self) -> ValueDescriptorVersion {
        self.version
    }

    /// Fully validate the recursive descriptor grammar.
    pub fn validate(self) -> Result<(), PackedValueError> {
        match self.version {
            ValueDescriptorVersion::V1 => v1::validate_descriptor(self),
        }
    }
}

/// An owned decoded value paired with its logical serialized length.
#[derive(Debug, PartialEq)]
pub struct DecodedPackedValue {
    /// The materialized Clarity value.
    pub value: Value,
    /// The length of its equivalent consensus serialization.
    pub consensus_byte_len: u32,
}

/// Immutable byte storage retained by borrowed packed values.
///
/// Implementations must return the same immutable bytes for their entire lifetime.
/// File-backed owners must prevent writes or truncation of exposed ranges.
pub trait PackedByteOwner: AsRef<[u8]> + Debug + Send + Sync {}

impl<T: AsRef<[u8]> + Debug + Send + Sync> PackedByteOwner for T {}

/// An admitted packed value backed by shared immutable record storage.
///
/// Logical list positions in an immutable source list.
#[derive(Clone, Debug)]
enum ListSelection {
    /// Consecutive elements selected without an index allocation.
    Range(Range<usize>),
    /// Retained source indices in ascending order.
    Indices(Arc<[u32]>),
}

impl ListSelection {
    /// Number of selected logical elements.
    fn len(&self) -> usize {
        match self {
            Self::Range(range) => range.len(),
            Self::Indices(indices) => indices.len(),
        }
    }

    /// Translate a logical position to the source position.
    fn source_index(&self, index: usize) -> Option<usize> {
        if index >= self.len() {
            return None;
        }
        Some(match self {
            Self::Range(range) => range.start + index,
            Self::Indices(indices) => indices[index] as usize,
        })
    }

    /// Compose selections into one level of source indices.
    fn through(self, parent: &Self) -> Self {
        match (&self, parent) {
            (Self::Range(child), Self::Range(parent)) => {
                Self::Range(parent.start + child.start..parent.start + child.end)
            }
            _ => Self::Indices(
                (0..self.len())
                    .map(|index| {
                        parent
                            .source_index(self.source_index(index).expect("selection index"))
                            .expect("validated parent index") as u32
                    })
                    .collect(),
            ),
        }
    }
}

/// Flattened list selection retaining the original byte owner.
#[derive(Clone, Debug)]
struct ListProjection {
    /// Source list with no further selection layer.
    source: SharedPackedValue,
    /// Logical positions selected from the source.
    selection: ListSelection,
}

/// Immutable packed records, projected values, or mixed runtime composites.
/// Clones retain child byte owners, including mapped extents, without constructing an owned [`Value`] tree.
#[derive(Clone, Debug)]
pub struct SharedPackedValue {
    /// Complete packed record bytes without the Binary V1 envelope.
    bytes: Arc<dyn PackedByteOwner>,
    /// Byte range of this projection's body within `bytes`.
    body_range: std::ops::Range<usize>,
    /// Declared schema used by typed projections.
    expected: TypeSignature,
    /// Epoch used for admission-compatible materialization.
    epoch: StacksEpochId,
    /// Equivalent consensus length supplied by the record or measured lazily for a child.
    consensus_byte_len: OnceLock<u32>,
    /// Lazily materialized compatibility value for unconverted VM consumers.
    materialized: OnceLock<Arc<Value>>,
    /// Lazily counted UTF-8 codepoints for repeated VM length/type/size requests.
    utf8_codepoints: OnceLock<usize>,
    /// Synthetic `some` result whose child retains the original record's bytes.
    optional_child: Option<Arc<SharedPackedValue>>,
    /// Virtual range or filtered list over retained source bytes.
    list_projection: Option<Arc<ListProjection>>,
    /// Mixed runtime aggregate retaining independent child owners.
    composite: Option<Arc<CompositeValue>>,
}

impl PartialEq for SharedPackedValue {
    fn eq(&self, other: &Self) -> bool {
        self.as_view().value_eq(other.as_view()).unwrap_or(false)
    }
}

impl Eq for SharedPackedValue {}

impl SharedPackedValue {
    /// Copy and validate one packed record obtained from transient storage.
    pub fn copy_from(
        bytes: &[u8],
        expected: &TypeSignature,
        epoch: &StacksEpochId,
    ) -> Result<Self, PackedValueError> {
        #[cfg(feature = "direct-value-diagnostics")]
        VALUE_READ_COUNTERS[0].fetch_add(bytes.len() as u64, Ordering::Relaxed);
        Self::from_owner(Arc::new(bytes.to_vec()), 0..bytes.len(), expected, epoch)
    }

    /// Validate a record within a retained immutable owner without copying its bytes.
    pub fn from_owner<O: PackedByteOwner + 'static>(
        bytes: Arc<O>,
        record_range: Range<usize>,
        expected: &TypeSignature,
        epoch: &StacksEpochId,
    ) -> Result<Self, PackedValueError> {
        let record = bytes
            .as_ref()
            .as_ref()
            .get(record_range.clone())
            .ok_or(PackedValueError::BorrowedView("record range exceeds owner"))?;
        #[cfg(feature = "direct-value-diagnostics")]
        {
            VALUE_READ_COUNTERS[1].fetch_add(1, Ordering::Relaxed);
            VALUE_READ_COUNTERS[2].fetch_add(record.len() as u64, Ordering::Relaxed);
        }
        let packed = PackedValueRef::parse(record)?;
        let view = PackedValueView::parse(packed, expected, epoch)?;
        Ok(Self {
            consensus_byte_len: OnceLock::from(view.consensus_byte_len()),
            body_range: record_range.start + packed.version().header_len()..record_range.end,
            bytes,
            expected: expected.clone(),
            epoch: *epoch,
            materialized: OnceLock::new(),
            utf8_codepoints: OnceLock::new(),
            optional_child: None,
            list_projection: None,
            composite: None,
        })
    }

    /// Retain encoder-produced bytes under the VM's admitted storage schema.
    ///
    /// The caller must supply immutable bytes produced by the packed encoder or a validated
    /// migration, and the schema used to store the value. This parses the envelope and checks
    /// owner bounds, but does not validate payload contents. Use `from_owner` for general inputs.
    pub fn from_encoded_owner<O: PackedByteOwner + 'static>(
        bytes: Arc<O>,
        record_range: Range<usize>,
        expected: &TypeSignature,
        epoch: &StacksEpochId,
    ) -> Result<Self, PackedValueError> {
        let record = bytes
            .as_ref()
            .as_ref()
            .get(record_range.clone())
            .ok_or(PackedValueError::BorrowedView("record range exceeds owner"))?;
        let packed = PackedValueRef::parse(record)?;
        Ok(Self {
            consensus_byte_len: OnceLock::from(packed.consensus_byte_len()),
            body_range: record_range.start + packed.version().header_len()..record_range.end,
            bytes,
            expected: expected.clone(),
            epoch: *epoch,
            materialized: OnceLock::new(),
            utf8_codepoints: OnceLock::new(),
            optional_child: None,
            list_projection: None,
            composite: None,
        })
    }

    /// Report whether a compatibility consumer has materialized this projection.
    #[cfg(any(test, feature = "testing"))]
    pub fn is_materialized(&self) -> bool {
        self.materialized.get().is_some()
    }

    /// Retain selected list elements without copying or decoding their payloads.
    pub fn filtered_list(self, indices: Vec<u32>) -> Result<Self, PackedValueError> {
        let length = self.as_view().as_list()?.len();
        if indices.iter().any(|index| *index as usize >= length)
            || indices.windows(2).any(|pair| pair[0] >= pair[1])
        {
            return Err(PackedValueError::BorrowedView("invalid filter selection"));
        }
        let expected = self.expected.clone();
        Ok(self.select_list(ListSelection::Indices(indices.into()), expected))
    }

    /// Build a flattened virtual list while preserving its logical result schema.
    fn select_list(self, selection: ListSelection, expected: TypeSignature) -> Self {
        let (source, selection) = match &self.list_projection {
            Some(parent) => (parent.source.clone(), selection.through(&parent.selection)),
            None => (self.clone(), selection),
        };
        Self {
            bytes: Arc::clone(&self.bytes),
            body_range: self.body_range.clone(),
            expected,
            epoch: self.epoch,
            consensus_byte_len: OnceLock::new(),
            materialized: OnceLock::new(),
            utf8_codepoints: OnceLock::new(),
            optional_child: None,
            list_projection: Some(Arc::new(ListProjection { source, selection })),
            composite: None,
        }
    }

    /// Retain a sequence range; return `None` when a list requires child type sanitization.
    pub fn sliced_sequence(
        &self,
        left: usize,
        right: usize,
    ) -> Result<Option<Self>, PackedValueError> {
        if let Some(value) = self.slice_segmented(left, right)? {
            return Ok(Some(value));
        }

        if let TypeSignature::SequenceType(SequenceSubtype::ListType(list_type)) = &self.expected {
            let view = self.as_view().as_list()?;
            if left > right || right > view.len() {
                return Err(PackedValueError::BorrowedView("invalid slice range"));
            }
            let item = list_type.get_list_item_type();
            let fixed_logical_type = matches!(
                item,
                TypeSignature::IntType
                    | TypeSignature::UIntType
                    | TypeSignature::BoolType
                    | TypeSignature::PrincipalType
                    | TypeSignature::CallableType(_)
                    | TypeSignature::TraitReferenceType(_)
                    | TypeSignature::TupleType(_)
                    | TypeSignature::SequenceType(SequenceSubtype::ListType(_))
            );
            if left == right || fixed_logical_type {
                let schema = if left == right {
                    TypeSignature::empty_list()
                } else {
                    ListTypeData::new_list(item.clone(), (right - left) as u32)?
                };
                return Ok(Some(self.clone().select_list(
                    ListSelection::Range(left..right),
                    TypeSignature::SequenceType(SequenceSubtype::ListType(schema)),
                )));
            }
            let types = (left..right)
                .map(|index| {
                    view.get(index)?
                        .ok_or(PackedValueError::BorrowedView("missing list child"))?
                        .logical_type()
                })
                .collect::<Result<Vec<_>, _>>()?;
            let schema = TypeSignature::parent_list_type(&types)?;
            if types.iter().any(|ty| ty != schema.get_list_item_type()) {
                return Ok(None);
            }
            return Ok(Some(self.clone().select_list(
                ListSelection::Range(left..right),
                TypeSignature::SequenceType(SequenceSubtype::ListType(schema)),
            )));
        }
        let view = self.as_view();
        let bytes = view
            .as_sequence_bytes()
            .ok_or(PackedValueError::BorrowedView("expected sequence"))?;
        let (range, expected) = match &self.expected {
            TypeSignature::SequenceType(SequenceSubtype::BufferType(_)) => (
                left..right,
                TypeSignature::SequenceType(SequenceSubtype::BufferType(BufferLength::try_from(
                    right
                        .checked_sub(left)
                        .ok_or(PackedValueError::SizeOverflow)?,
                )?)),
            ),
            TypeSignature::SequenceType(SequenceSubtype::StringType(StringSubtype::ASCII(_))) => (
                left..right,
                TypeSignature::SequenceType(SequenceSubtype::StringType(StringSubtype::ASCII(
                    BufferLength::try_from(
                        right
                            .checked_sub(left)
                            .ok_or(PackedValueError::SizeOverflow)?,
                    )?,
                ))),
            ),
            TypeSignature::SequenceType(SequenceSubtype::StringType(StringSubtype::UTF8(_))) => {
                if left > right {
                    return Err(PackedValueError::BorrowedView("invalid slice range"));
                }
                let mut boundaries = bytes
                    .iter()
                    .enumerate()
                    .filter_map(|(index, byte)| (byte & 0xc0 != 0x80).then_some(index))
                    .chain(iter::once(bytes.len()));
                let start = boundaries
                    .nth(left)
                    .ok_or(PackedValueError::BorrowedView("invalid slice start"))?;
                let end = if right == left {
                    start
                } else {
                    boundaries
                        .nth(right - left - 1)
                        .ok_or(PackedValueError::BorrowedView("invalid slice end"))?
                };
                (
                    start..end,
                    TypeSignature::SequenceType(SequenceSubtype::StringType(StringSubtype::UTF8(
                        StringUTF8Length::try_from(right - left)?,
                    ))),
                )
            }
            _ => return Err(PackedValueError::BorrowedView("expected sequence")),
        };
        if range.end > bytes.len() {
            return Err(PackedValueError::BorrowedView("invalid slice end"));
        }
        Ok(Some(Self {
            bytes: Arc::clone(&self.bytes),
            body_range: self.body_range.start + range.start..self.body_range.start + range.end,
            expected,
            epoch: self.epoch,
            consensus_byte_len: OnceLock::new(),
            materialized: OnceLock::new(),
            utf8_codepoints: OnceLock::new(),
            optional_child: None,
            list_projection: None,
            composite: None,
        }))
    }

    /// Wrap a projection in `some` without copying or encoding its payload.
    pub fn into_optional(self) -> Result<Self, PackedValueError> {
        let expected = TypeSignature::new_option(self.logical_type()?)?;
        Ok(Self {
            bytes: Arc::clone(&self.bytes),
            body_range: self.body_range.clone(),
            expected,
            epoch: self.epoch,
            consensus_byte_len: OnceLock::new(),
            materialized: OnceLock::new(),
            utf8_codepoints: OnceLock::new(),
            optional_child: Some(Arc::new(self)),
            list_projection: None,
            composite: None,
        })
    }

    /// Project the active optional child while retaining the shared record allocation.
    pub fn optional_child(&self) -> Result<Option<Self>, PackedValueError> {
        if let Some(child) = &self.optional_child {
            return Ok(Some(child.as_ref().clone()));
        }
        self.as_view()
            .optional_child()?
            .map(|view| self.project_view(view))
            .transpose()
    }

    /// Project the active response child and report whether it is the `ok` branch.
    pub fn response_child(&self) -> Result<(bool, Self), PackedValueError> {
        if let Some(composite) = &self.composite {
            if let CompositeValue::Response(committed, child) = composite.as_ref() {
                return Ok((*committed, child.clone()));
            }
        }
        let (committed, child) = self.as_view().response_child()?;
        Ok((committed, self.project_view(child)?))
    }

    /// Project one tuple field while retaining the shared record allocation.
    pub fn tuple_field(&self, name: &str) -> Result<Option<Self>, PackedValueError> {
        if let Some(composite) = &self.composite {
            if let CompositeValue::Tuple(fields) = composite.as_ref() {
                return Ok(fields
                    .binary_search_by(|(key, _)| key.as_str().cmp(name))
                    .ok()
                    .map(|i| fields[i].1.clone()));
            }
        }
        self.as_view()
            .as_tuple()?
            .get(name)?
            .map(|view| self.project_view(view))
            .transpose()
    }

    /// Retain a non-lane list child in the same immutable byte owner.
    /// Integer and Boolean lanes must be read as inline scalars through the list view.
    pub fn list_child(&self, index: usize) -> Result<Option<Self>, PackedValueError> {
        if let Some(projection) = &self.list_projection {
            return projection
                .selection
                .source_index(index)
                .map(|i| projection.source.list_child(i))
                .transpose()
                .map(Option::flatten);
        }
        if let Some(composite) = &self.composite {
            if let CompositeValue::List(list) = composite.as_ref() {
                return list.child(index);
            }
        }
        self.as_view()
            .as_list()?
            .get(index)?
            .map(|view| self.project_view(view))
            .transpose()
    }

    /// Return the number of tuple fields without materializing them.
    pub fn tuple_len(&self) -> Result<usize, PackedValueError> {
        self.as_view().as_tuple().map(|tuple| tuple.len())
    }

    /// Project one contiguous child view into another shared value.
    fn project_view(&self, view: PackedValueView<'_>) -> Result<Self, PackedValueError> {
        stacks_profiler::diagnostics::count("borrowed_child_projections", 1);
        #[cfg(feature = "direct-value-diagnostics")]
        VALUE_READ_COUNTERS[5].fetch_add(1, Ordering::Relaxed);
        let Some(body) = view.encoded_body() else {
            return Self::from_value(view.to_owned_value()?, &self.epoch);
        };
        let base = self.bytes.as_ref().as_ref().as_ptr() as usize;
        let start =
            (body.as_ptr() as usize)
                .checked_sub(base)
                .ok_or(PackedValueError::BorrowedView(
                    "projected body does not belong to shared record",
                ))?;
        let end = start
            .checked_add(body.len())
            .ok_or(PackedValueError::SizeOverflow)?;
        if end > self.bytes.as_ref().as_ref().len() {
            return Err(PackedValueError::BorrowedView(
                "projected body exceeds shared record",
            ));
        }
        Ok(Self {
            bytes: Arc::clone(&self.bytes),
            body_range: start..end,
            expected: view.expected().clone(),
            epoch: self.epoch,
            consensus_byte_len: view
                .known_consensus_byte_len()
                .map(OnceLock::from)
                .unwrap_or_default(),
            materialized: OnceLock::new(),
            utf8_codepoints: OnceLock::new(),
            optional_child: None,
            list_projection: None,
            composite: None,
        })
    }

    /// Return the equivalent consensus-serialized byte length.
    pub fn consensus_byte_len(&self) -> u32 {
        *self
            .consensus_byte_len
            .get_or_init(|| self.as_view().consensus_byte_len())
    }

    /// Return the materialized value's logical VM size without constructing it.
    pub fn logical_size(&self) -> Result<u32, PackedValueError> {
        if let Some(count) = self.utf8_len() {
            return u32::try_from(count)
                .map_err(|_| PackedValueError::SizeOverflow)?
                .checked_mul(4)
                .and_then(|size| size.checked_add(4))
                .ok_or(PackedValueError::SizeOverflow);
        }
        self.as_view().logical_size()
    }

    /// Derive the type carried by the equivalent materialized value.
    pub fn logical_type(&self) -> Result<TypeSignature, PackedValueError> {
        if let Some(count) = self.utf8_len() {
            return Ok(TypeSignature::SequenceType(SequenceSubtype::StringType(
                StringSubtype::UTF8(StringUTF8Length::try_from(count)?),
            )));
        }
        self.as_view().logical_type()
    }

    /// Return and cache the codepoint count for admitted UTF-8 values.
    pub fn utf8_len(&self) -> Option<usize> {
        if !matches!(
            self.expected,
            TypeSignature::SequenceType(SequenceSubtype::StringType(StringSubtype::UTF8(_)))
        ) {
            return None;
        }
        Some(
            *self
                .utf8_codepoints
                .get_or_init(|| self.as_view().utf8_len().expect("UTF-8 type")),
        )
    }

    /// Return the declared type used to interpret this projection.
    pub fn expected(&self) -> &TypeSignature {
        &self.expected
    }

    /// Return this projection's active logical kind.
    pub fn kind(&self) -> Result<PackedValueKind, PackedValueError> {
        self.as_view().kind()
    }

    /// Materialize this value in the existing owned representation.
    #[cfg_attr(any(test, feature = "testing"), track_caller)]
    pub fn to_owned_value(&self) -> Result<Value, PackedValueError> {
        #[cfg(any(test, feature = "testing"))]
        {
            let caller = Location::caller();
            SHARED_MATERIALIZATION_LOCATIONS.with(|locations| locations.borrow_mut().push(caller));
        }
        self.decode_owned_value()
    }

    /// Materialize once and borrow the cached existing representation.
    #[cfg_attr(any(test, feature = "testing"), track_caller)]
    pub fn materialized(&self) -> Result<&Value, PackedValueError> {
        if let Some(value) = self.materialized.get() {
            return Ok(value);
        }
        #[cfg(any(test, feature = "testing"))]
        {
            let caller = Location::caller();
            SHARED_MATERIALIZATION_LOCATIONS.with(|locations| locations.borrow_mut().push(caller));
        }
        let value = self.decode_owned_value()?;
        let _ = self.materialized.set(Arc::new(value));
        Ok(self
            .materialized
            .get()
            .expect("materialized value was initialized above"))
    }

    /// Borrow the compatibility value from a validated or encoder-produced record.
    ///
    /// Failure indicates a codec disagreement or a violated trusted-admission contract.
    #[cfg_attr(any(test, feature = "testing"), track_caller)]
    pub fn materialized_infallible(&self) -> &Value {
        self.materialized()
            .expect("admitted packed record must remain decodable")
    }

    /// Decode the equivalent owned value without recording a compatibility boundary.
    fn decode_owned_value(&self) -> Result<Value, PackedValueError> {
        let _materialize = stacks_profiler::diagnostic_span!("Value: Materialize");
        stacks_profiler::diagnostics::count("materializations", 1);
        stacks_profiler::diagnostics::count(
            "materialized_source_bytes",
            self.body_range.len() as u64,
        );
        #[cfg(feature = "direct-value-diagnostics")]
        {
            VALUE_READ_COUNTERS[3].fetch_add(1, Ordering::Relaxed);
            VALUE_READ_COUNTERS[4]
                .fetch_add(u64::from(self.consensus_byte_len()), Ordering::Relaxed);
        }
        self.as_view().to_owned_value()
    }

    /// Return the number of strong owners of this value's record allocation.
    #[cfg(any(test, feature = "testing"))]
    pub fn record_owner_count(&self) -> usize {
        Arc::strong_count(&self.bytes)
    }

    /// Reset the current test thread's shared-value materialization counter.
    #[cfg(any(test, feature = "testing"))]
    pub fn reset_materialization_count() {
        SHARED_MATERIALIZATION_LOCATIONS.with(|locations| locations.borrow_mut().clear());
    }

    /// Return shared-value materializations recorded by the current test thread.
    #[cfg(any(test, feature = "testing"))]
    pub fn materialization_count() -> u64 {
        SHARED_MATERIALIZATION_LOCATIONS.with(|locations| locations.borrow().len() as u64)
    }

    /// Return call sites that materialized shared values on the current test thread.
    #[cfg(any(test, feature = "testing"))]
    pub fn materialization_locations() -> Vec<String> {
        SHARED_MATERIALIZATION_LOCATIONS
            .with(|locations| locations.borrow().iter().map(ToString::to_string).collect())
    }
}

impl SharedPackedValue {
    /// Borrow this shared record without materializing its contents.
    pub fn as_view(&self) -> PackedValueView<'_> {
        if let Some(composite) = &self.composite {
            return PackedValueView::composite(composite, &self.expected, &self.epoch);
        }
        if self.list_projection.is_some() {
            return PackedValueView::projected_list(self, &self.expected, &self.epoch);
        }
        if let Some(child) = &self.optional_child {
            return PackedValueView::some_child(child, &self.expected, &self.epoch);
        }
        PackedValueView::from_admitted_body(
            &self.bytes.as_ref().as_ref()[self.body_range.clone()],
            &self.expected,
            &self.epoch,
            self.consensus_byte_len.get().copied(),
        )
    }
}

#[cfg(test)]
mod sequence_projection_tests {
    use super::*;
    use crate::types::{ListData, SequenceData};

    /// Produce a shared fixture with the exact logical schema of the original value.
    fn shared(value: &Value) -> SharedPackedValue {
        let packed = PackedValue::encode(PackedValueVersion::V1, value).unwrap();
        SharedPackedValue::copy_from(
            packed.as_bytes(),
            &TypeSignature::type_of(value).unwrap(),
            &StacksEpochId::latest(),
        )
        .unwrap()
    }

    /// Repeated selection stays one level deep and preserves selected payload addresses.
    #[test]
    fn filtered_lists_flatten_and_preserve_bounds_and_owners() {
        let epoch = StacksEpochId::latest();
        let values = vec![
            Value::buff_from(vec![1; 4096]).unwrap(),
            Value::buff_from(vec![2; 4096]).unwrap(),
            Value::buff_from(vec![3; 4096]).unwrap(),
        ];
        let list = Value::cons_list(values.clone(), &epoch).unwrap();
        let mut selected = shared(&list);
        let pointer = selected
            .as_view()
            .as_list()
            .unwrap()
            .get(2)
            .unwrap()
            .unwrap()
            .as_sequence_bytes()
            .unwrap()
            .as_ptr();
        selected = selected
            .filtered_list(vec![0, 2])
            .unwrap()
            .sliced_sequence(1, 2)
            .unwrap()
            .unwrap();
        for _ in 0..1000 {
            selected = selected.filtered_list(vec![0]).unwrap();
        }
        let projection = selected.list_projection.as_ref().unwrap();
        assert!(projection.source.list_projection.is_none());
        assert_eq!(
            selected
                .as_view()
                .as_list()
                .unwrap()
                .get(0)
                .unwrap()
                .unwrap()
                .as_sequence_bytes()
                .unwrap()
                .as_ptr(),
            pointer
        );
        let expected = Value::cons_list(vec![values[2].clone()], &epoch).unwrap();
        assert_eq!(selected.logical_size().unwrap(), expected.size().unwrap());
        assert_eq!(
            selected.consensus_byte_len(),
            expected.serialized_size().unwrap()
        );
        assert_eq!(selected.to_owned_value().unwrap(), expected);

        let original = shared(&list);
        let TypeSignature::SequenceType(SequenceSubtype::ListType(schema)) =
            original.expected().clone()
        else {
            unreachable!()
        };
        let filtered = original.filtered_list(vec![2]).unwrap();
        let expected = Value::Sequence(SequenceData::List(ListData {
            data: vec![values[2].clone()],
            type_signature: schema,
        }));
        assert_eq!(filtered.logical_size().unwrap(), expected.size().unwrap());
        assert_eq!(filtered.to_owned_value().unwrap(), expected);
        let empty = filtered.filtered_list(vec![]).unwrap();
        assert_eq!(empty.as_view().as_list().unwrap().len(), 0);
        assert_eq!(empty.consensus_byte_len(), 5);
        assert!(shared(&list).filtered_list(vec![2, 1]).is_err());
        assert!(shared(&list).filtered_list(vec![3]).is_err());
    }

    /// Byte and UTF-8 slices alias the source, including after the parent is dropped.
    #[test]
    fn byte_slices_retain_source_ranges() {
        for value in [
            Value::buff_from(vec![1, 2, 3, 4]).unwrap(),
            Value::string_ascii_from_bytes(b"abcd".to_vec()).unwrap(),
            Value::string_utf8_from_bytes("aé😀z".as_bytes().to_vec()).unwrap(),
        ] {
            let original = shared(&value);
            let pointer = original.as_view().as_sequence_bytes().unwrap().as_ptr();
            let slice = original.sliced_sequence(1, 3).unwrap().unwrap();
            assert_eq!(
                slice.as_view().as_sequence_bytes().unwrap().as_ptr(),
                pointer.wrapping_add(1)
            );
            drop(original);
            let Value::Sequence(sequence) = value else {
                unreachable!()
            };
            let expected = sequence.slice(&StacksEpochId::latest(), 1, 3).unwrap();
            assert_eq!(
                slice.logical_type().unwrap(),
                TypeSignature::type_of(&expected).unwrap()
            );
            assert_eq!(slice.to_owned_value().unwrap(), expected);
        }
    }

    /// Heterogeneous result schemas retain the original sanitizing construction boundary.
    #[test]
    fn heterogeneous_list_slice_requests_sanitization() {
        let list = Value::cons_list(
            vec![
                Value::buff_from(vec![1]).unwrap(),
                Value::buff_from(vec![2; 16]).unwrap(),
            ],
            &StacksEpochId::latest(),
        )
        .unwrap();
        let original = shared(&list);
        assert!(original.sliced_sequence(0, 2).unwrap().is_none());
        let one = original.sliced_sequence(0, 1).unwrap().unwrap();
        assert_eq!(
            one.to_owned_value().unwrap(),
            Value::cons_list(
                vec![Value::buff_from(vec![1]).unwrap()],
                &StacksEpochId::latest()
            )
            .unwrap()
        );
        let empty = original.sliced_sequence(0, 0).unwrap().unwrap();
        assert_eq!(
            empty.to_owned_value().unwrap(),
            Value::cons_list(vec![], &StacksEpochId::latest()).unwrap()
        );
    }
}
