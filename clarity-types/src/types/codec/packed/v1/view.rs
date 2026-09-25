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

//! Validated borrowed views over canonical packed values.

use std::io::{self, Write};
use std::str;

use stacks_common::types::StacksEpochId;

use crate::types::codec::packed::SharedPackedValue;
use crate::types::codec::packed::composite::{CompositeValue, SharedList};

use super::{PackedValueError, PackedValueRef, decode, directory, layout, primitive};
use crate::representations::ClarityName;
use crate::types::signatures::{
    BufferLength, CallableSubtype, SequenceSubtype, StringSubtype, StringUTF8Length,
};
use crate::types::{
    CharType, ListTypeData, PrincipalData, QualifiedContractIdentifier, SequenceData,
    StandardPrincipalData, TraitIdentifier, TypeSignature, Value,
};

/// The active logical kind exposed by a packed value view.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PackedValueKind {
    /// Signed 128-bit integer.
    Int,
    /// Unsigned 128-bit integer.
    UInt,
    /// Boolean.
    Bool,
    /// Byte buffer.
    Buffer,
    /// ASCII string.
    Ascii,
    /// UTF-8 string.
    Utf8,
    /// Standard or contract principal.
    Principal,
    /// Callable contract.
    Callable,
    /// Optional value.
    Optional,
    /// Response value.
    Response,
    /// Tuple value.
    Tuple,
    /// List value.
    List,
}

/// Borrowed identity components for a packed principal or callable contract.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PackedPrincipalView<'a> {
    /// A standard principal's network/version byte and 20-byte hash.
    Standard {
        /// Principal address version.
        version: u8,
        /// Borrowed address hash bytes.
        hash: &'a [u8; 20],
    },
    /// A contract principal's issuer and validated contract name.
    Contract {
        /// Issuer address version.
        issuer_version: u8,
        /// Borrowed issuer hash bytes.
        issuer_hash: &'a [u8; 20],
        /// Borrowed validated contract name.
        name: &'a str,
    },
}

impl<'a> PackedPrincipalView<'a> {
    /// Convert the codec's validated physical principal into public borrowed components.
    fn from_packed(principal: primitive::PackedPrincipal<'a>) -> Self {
        match principal {
            primitive::PackedPrincipal::Standard(bytes) => Self::Standard {
                version: bytes[0],
                hash: bytes[1..]
                    .try_into()
                    .expect("validated standard principal has a 20-byte hash"),
            },
            primitive::PackedPrincipal::Contract { issuer, name } => Self::Contract {
                issuer_version: issuer[0],
                issuer_hash: issuer[1..]
                    .try_into()
                    .expect("validated contract issuer has a 20-byte hash"),
                name,
            },
        }
    }
}

/// Physical representation of one projected packed value.
#[derive(Clone, Copy, Debug)]
enum ViewBody<'a> {
    /// Canonical packed body bytes.
    Encoded(&'a [u8]),
    /// Scalar projected from a homogeneous unsigned lane.
    UInt(u128),
    /// Scalar projected from a homogeneous signed lane.
    Int(i128),
    /// Scalar projected from a bit-packed Boolean lane.
    Bool(bool),
    /// In-memory optional wrapper retaining a shared child without encoding it again.
    Some(&'a SharedPackedValue),
    /// A virtual selection from a shared source list.
    List(&'a SharedPackedValue),
    /// Runtime aggregate of independently retained child views.
    Composite(&'a CompositeValue),
}

/// A completely validated borrowed view over one packed Clarity value.
#[derive(Clone, Copy, Debug)]
pub struct PackedValueView<'a> {
    /// Packed body or decoded lane scalar.
    body: ViewBody<'a>,
    /// Declared schema used to interpret the packed body.
    expected: &'a TypeSignature,
    /// Epoch used for admission-compatible materialization.
    epoch: &'a StacksEpochId,
    /// Equivalent consensus-serialized length.
    consensus_byte_len: Option<u32>,
}

impl<'a> PackedValueView<'a> {
    /// Parse and allocation-free validate one complete packed record.
    pub fn parse(
        packed: PackedValueRef<'a>,
        expected: &'a TypeSignature,
        epoch: &'a StacksEpochId,
    ) -> Result<Self, PackedValueError> {
        let body = packed.body();
        let consensus_byte_len = validate_body(body, expected, epoch)?;
        if consensus_byte_len != packed.consensus_byte_len() {
            return Err(PackedValueError::BorrowedView(
                "canonical logical consensus length mismatch",
            ));
        }
        Ok(Self {
            body: ViewBody::Encoded(body),
            expected,
            epoch,
            consensus_byte_len: Some(consensus_byte_len),
        })
    }

    /// Construct a view over a record validated by its stable owner.
    pub fn from_validated(
        body: &'a [u8],
        expected: &'a TypeSignature,
        epoch: &'a StacksEpochId,
        consensus_byte_len: u32,
    ) -> Self {
        Self {
            body: ViewBody::Encoded(body),
            expected,
            epoch,
            consensus_byte_len: Some(consensus_byte_len),
        }
    }

    /// Return this value's active logical kind.
    pub fn kind(self) -> Result<PackedValueKind, PackedValueError> {
        use TypeSignature::*;

        Ok(match self.expected {
            IntType => PackedValueKind::Int,
            UIntType => PackedValueKind::UInt,
            BoolType => PackedValueKind::Bool,
            PrincipalType => PackedValueKind::Principal,
            CallableType(_) | TraitReferenceType(_) => PackedValueKind::Callable,
            OptionalType(_) => PackedValueKind::Optional,
            ResponseType(_) => PackedValueKind::Response,
            TupleType(_) => PackedValueKind::Tuple,
            SequenceType(SequenceSubtype::BufferType(_)) => PackedValueKind::Buffer,
            SequenceType(SequenceSubtype::StringType(StringSubtype::ASCII(_))) => {
                PackedValueKind::Ascii
            }
            SequenceType(SequenceSubtype::StringType(StringSubtype::UTF8(_))) => {
                PackedValueKind::Utf8
            }
            SequenceType(SequenceSubtype::ListType(_)) => PackedValueKind::List,
            NoType => return Err(PackedValueError::BorrowedView("NoType cannot be active")),
            ListUnionType(_) => {
                return Err(PackedValueError::BorrowedView(
                    "ListUnionType is analysis-only",
                ));
            }
        })
    }

    /// Bound packed body bytes from below, inspecting at most `visits` logical nodes.
    /// Stops at `cap`; omitted directories and unvisited children only weaken the bound.
    pub fn body_len_lower_bound(
        self,
        cap: usize,
        visits: &mut usize,
    ) -> Result<usize, PackedValueError> {
        if cap == 0 || *visits == 0 {
            return Ok(0);
        }
        *visits -= 1;
        let bytes = match self.kind()? {
            PackedValueKind::Int => primitive::packed_int_width(self.as_int().expect("int view")),
            PackedValueKind::UInt => {
                primitive::packed_uint_width(self.as_uint().expect("uint view"))
            }
            PackedValueKind::Bool => 1,
            PackedValueKind::Buffer | PackedValueKind::Ascii | PackedValueKind::Utf8 => {
                self.sequence_byte_len().expect("byte sequence view")
            }
            PackedValueKind::Principal | PackedValueKind::Callable => {
                match self.as_principal()?.expect("principal view") {
                    PackedPrincipalView::Standard { .. } => 22,
                    PackedPrincipalView::Contract { name, .. } => 22 + name.len(),
                }
            }
            PackedValueKind::Optional => {
                1 + match self.optional_child()? {
                    Some(child) => child.body_len_lower_bound(cap - 1, visits)?,
                    None => 0,
                }
            }
            PackedValueKind::Response => {
                1 + self
                    .response_child()?
                    .1
                    .body_len_lower_bound(cap - 1, visits)?
            }
            PackedValueKind::Tuple => {
                let tuple = self.as_tuple()?;
                let mut bytes = 0;
                for index in 0..tuple.len() {
                    if bytes >= cap || *visits == 0 {
                        break;
                    }
                    let (_, child) = tuple.get_index(index)?.expect("in-bounds tuple field");
                    bytes += child.body_len_lower_bound(cap - bytes, visits)?;
                }
                bytes
            }
            PackedValueKind::List => {
                let list = self.as_list()?;
                // Integer lanes need at least one byte per item; Boolean lanes pack eight.
                match list.expected.get_list_item_type() {
                    TypeSignature::IntType | TypeSignature::UIntType => {
                        4usize.saturating_add(list.len())
                    }
                    TypeSignature::BoolType => 4 + list.len().div_ceil(8),
                    _ => {
                        let mut bytes = 4;
                        for index in 0..list.len() {
                            if bytes >= cap || *visits == 0 {
                                break;
                            }
                            let child = list.get(index)?.expect("in-bounds list item");
                            bytes += child.body_len_lower_bound(cap - bytes, visits)?;
                        }
                        bytes
                    }
                }
            }
        };
        Ok(bytes.min(cap))
    }

    /// Return the equivalent consensus-serialized byte length.
    pub fn consensus_byte_len(self) -> u32 {
        self.consensus_byte_len.unwrap_or_else(|| match self.body {
            ViewBody::Encoded(bytes) => measure_body(bytes, self.expected, self.epoch)
                .expect("admitted packed bytes retain valid framing"),
            ViewBody::UInt(_) | ViewBody::Int(_) => 17,
            ViewBody::Bool(_) => 1,
            ViewBody::Some(child) => 1 + child.consensus_byte_len(),
            ViewBody::Composite(CompositeValue::Bytes { tree, .. }) => 5 + tree.byte_len() as u32,
            ViewBody::Composite(CompositeValue::Response(_, child)) => {
                1 + child.consensus_byte_len()
            }
            ViewBody::Composite(CompositeValue::Tuple(fields)) => {
                fields.iter().fold(5, |size, (name, child)| {
                    size + 1 + name.len() as u32 + child.consensus_byte_len()
                })
            }
            ViewBody::List(_) | ViewBody::Composite(CompositeValue::List(_)) => {
                let list = self.as_list().expect("projected list");
                (0..list.len()).fold(5, |size, index| {
                    size + list
                        .get(index)
                        .expect("list child")
                        .expect("list index")
                        .consensus_byte_len()
                })
            }
        })
    }

    /// Return the serialization length when it is already known without a body walk.
    pub const fn known_consensus_byte_len(self) -> Option<u32> {
        self.consensus_byte_len
    }

    /// Borrow admitted bytes without rescanning their contents.
    ///
    /// The caller must supply immutable, encoder-produced or validated bytes matching
    /// `expected`, and an accurate consensus length when one is supplied.
    pub fn from_admitted_body(
        body: &'a [u8],
        expected: &'a TypeSignature,
        epoch: &'a StacksEpochId,
        consensus_byte_len: Option<u32>,
    ) -> Self {
        Self {
            body: ViewBody::Encoded(body),
            expected,
            epoch,
            consensus_byte_len,
        }
    }

    /// View a synthetic optional wrapper over an existing shared child.
    pub fn some_child(
        child: &'a SharedPackedValue,
        expected: &'a TypeSignature,
        epoch: &'a StacksEpochId,
    ) -> Self {
        Self {
            body: ViewBody::Some(child),
            expected,
            epoch,
            consensus_byte_len: None,
        }
    }

    /// View a virtual list whose element selection is retained by its owner.
    pub fn projected_list(
        owner: &'a SharedPackedValue,
        expected: &'a TypeSignature,
        epoch: &'a StacksEpochId,
    ) -> Self {
        Self {
            body: ViewBody::List(owner),
            expected,
            epoch,
            consensus_byte_len: None,
        }
    }

    /// Borrow an admitted mixed runtime aggregate.
    pub fn composite(
        value: &'a CompositeValue,
        expected: &'a TypeSignature,
        epoch: &'a StacksEpochId,
    ) -> Self {
        Self {
            body: ViewBody::Composite(value),
            expected,
            epoch,
            consensus_byte_len: None,
        }
    }

    /// Return the in-memory logical size charged by [`Value::size`] without materializing.
    pub fn logical_size(self) -> Result<u32, PackedValueError> {
        use TypeSignature::*;

        match self.expected {
            IntType | UIntType => Ok(16),
            BoolType => Ok(1),
            PrincipalType => Ok(148),
            CallableType(_) | TraitReferenceType(_) => self.expected.size().map_err(Into::into),
            SequenceType(SequenceSubtype::BufferType(_))
            | SequenceType(SequenceSubtype::StringType(StringSubtype::ASCII(_))) => {
                let length = self
                    .sequence_byte_len()
                    .ok_or(PackedValueError::BorrowedView("type mismatch"))?;
                u32::try_from(length)
                    .map_err(|_| PackedValueError::SizeOverflow)?
                    .checked_add(4)
                    .ok_or(PackedValueError::SizeOverflow)
            }
            SequenceType(SequenceSubtype::StringType(StringSubtype::UTF8(_))) => {
                let characters = self
                    .utf8_len()
                    .ok_or(PackedValueError::BorrowedView("type mismatch"))?;
                let characters =
                    u32::try_from(characters).map_err(|_| PackedValueError::SizeOverflow)?;
                characters
                    .checked_mul(4)
                    .and_then(|size| size.checked_add(4))
                    .ok_or(PackedValueError::SizeOverflow)
            }
            OptionalType(_) => match self.optional_child()? {
                Some(child) => primitive::checked_logical_add(1, child.logical_size()?),
                None => Ok(2),
            },
            ResponseType(_) => {
                let (_, child) = self.response_child()?;
                primitive::checked_logical_add(1, child.logical_size()?.max(1))
            }
            TupleType(_) | SequenceType(SequenceSubtype::ListType(_)) => {
                self.expected.size().map_err(Into::into)
            }
            NoType => Err(PackedValueError::BorrowedView("NoType cannot be active")),
            ListUnionType(_) => Err(PackedValueError::BorrowedView(
                "ListUnionType is analysis-only",
            )),
        }
    }

    /// Derive the type carried by the equivalent materialized [`Value`].
    pub fn logical_type(self) -> Result<TypeSignature, PackedValueError> {
        use TypeSignature::*;

        Ok(match self.expected {
            IntType => IntType,
            UIntType => UIntType,
            BoolType => BoolType,
            PrincipalType => PrincipalType,
            CallableType(_) | TraitReferenceType(_) => self.expected.clone(),
            SequenceType(SequenceSubtype::BufferType(_)) => {
                SequenceType(SequenceSubtype::BufferType(BufferLength::try_from(
                    self.sequence_byte_len()
                        .ok_or(PackedValueError::BorrowedView("type mismatch"))?,
                )?))
            }
            SequenceType(SequenceSubtype::StringType(StringSubtype::ASCII(_))) => SequenceType(
                SequenceSubtype::StringType(StringSubtype::ASCII(BufferLength::try_from(
                    self.sequence_byte_len()
                        .ok_or(PackedValueError::BorrowedView("type mismatch"))?,
                )?)),
            ),
            SequenceType(SequenceSubtype::StringType(StringSubtype::UTF8(_))) => SequenceType(
                SequenceSubtype::StringType(StringSubtype::UTF8(StringUTF8Length::try_from(
                    self.utf8_len()
                        .ok_or(PackedValueError::BorrowedView("type mismatch"))?,
                )?)),
            ),
            OptionalType(_) => TypeSignature::new_option(match self.optional_child()? {
                Some(child) => child.logical_type()?,
                None => NoType,
            })?,
            ResponseType(_) => {
                let (committed, child) = self.response_child()?;
                if committed {
                    TypeSignature::new_response(child.logical_type()?, NoType)?
                } else {
                    TypeSignature::new_response(NoType, child.logical_type()?)?
                }
            }
            TupleType(_) | SequenceType(SequenceSubtype::ListType(_)) => self.expected.clone(),
            NoType => return Err(PackedValueError::BorrowedView("NoType cannot be active")),
            ListUnionType(_) => {
                return Err(PackedValueError::BorrowedView(
                    "ListUnionType is analysis-only",
                ));
            }
        })
    }

    /// Return this view's declared schema.
    pub const fn expected(self) -> &'a TypeSignature {
        self.expected
    }

    /// Return the encoded body bytes when this view is backed by a contiguous record slice.
    /// Integer and Boolean lane projections return `None` because their scalar does not occupy a
    /// standalone encoded slice.
    pub const fn encoded_body(self) -> Option<&'a [u8]> {
        match self.body {
            ViewBody::Encoded(bytes) => Some(bytes),
            ViewBody::UInt(_)
            | ViewBody::Int(_)
            | ViewBody::Bool(_)
            | ViewBody::Some(_)
            | ViewBody::List(_)
            | ViewBody::Composite(_) => None,
        }
    }

    /// Return this value as an unsigned integer.
    pub fn as_uint(self) -> Option<u128> {
        match (self.expected, self.body) {
            (TypeSignature::UIntType, ViewBody::Encoded(bytes)) => {
                primitive::decode_admitted_u128(bytes)
            }
            (TypeSignature::UIntType, ViewBody::UInt(value)) => Some(value),
            _ => None,
        }
    }

    /// Return this value as a signed integer.
    pub fn as_int(self) -> Option<i128> {
        match (self.expected, self.body) {
            (TypeSignature::IntType, ViewBody::Encoded(bytes)) => {
                primitive::decode_admitted_i128(bytes)
            }
            (TypeSignature::IntType, ViewBody::Int(value)) => Some(value),
            _ => None,
        }
    }

    /// Return this value as a Boolean.
    pub fn as_bool(self) -> Option<bool> {
        match (self.expected, self.body) {
            (TypeSignature::BoolType, ViewBody::Encoded([value])) if *value <= 1 => {
                Some(*value == 1)
            }
            (TypeSignature::BoolType, ViewBody::Bool(value)) => Some(value),
            _ => None,
        }
    }

    /// Borrow this buffer or string's contiguous packed payload.
    pub fn as_sequence_bytes(self) -> Option<&'a [u8]> {
        if let ViewBody::Composite(value) = self.body {
            return value.sequence_bytes();
        }
        match (self.expected, self.body) {
            (
                TypeSignature::SequenceType(
                    SequenceSubtype::BufferType(_) | SequenceSubtype::StringType(_),
                ),
                ViewBody::Encoded(bytes),
            ) => Some(bytes),
            _ => None,
        }
    }

    /// Physical byte count, without coalescing a segmented payload.
    pub fn sequence_byte_len(self) -> Option<usize> {
        if let ViewBody::Composite(CompositeValue::Bytes { tree, .. }) = self.body {
            return Some(tree.byte_len());
        }
        self.as_sequence_bytes().map(<[u8]>::len)
    }

    /// Write byte segments in logical order without allocating a contiguous copy.
    pub fn write_sequence_bytes<W: Write>(self, writer: &mut W) -> io::Result<()> {
        if let ViewBody::Composite(CompositeValue::Bytes { tree, .. }) = self.body {
            return tree.write_bytes(writer);
        }
        writer.write_all(self.as_sequence_bytes().expect("byte sequence"))
    }

    /// Count admitted UTF-8 codepoints without validating or decoding them first.
    pub fn utf8_len(self) -> Option<usize> {
        if !matches!(
            self.expected,
            TypeSignature::SequenceType(SequenceSubtype::StringType(StringSubtype::UTF8(_)))
        ) {
            return None;
        }
        if let ViewBody::Composite(CompositeValue::Bytes { tree, .. }) = self.body {
            return Some(tree.len());
        }
        Some(
            self.as_sequence_bytes()?
                .iter()
                .filter(|byte| **byte & 0xc0 != 0x80)
                .count(),
        )
    }

    /// Borrow only the selected admitted UTF-8 codepoint, stopping at its following boundary.
    pub fn utf8_element(self, index: usize) -> Option<&'a [u8]> {
        if !matches!(
            self.expected,
            TypeSignature::SequenceType(SequenceSubtype::StringType(StringSubtype::UTF8(_)))
        ) {
            return None;
        }
        let bytes = self.as_sequence_bytes()?;
        let mut starts = bytes
            .iter()
            .enumerate()
            .filter_map(|(i, byte)| (byte & 0xc0 != 0x80).then_some(i));
        let start = starts.nth(index)?;
        let end = starts.next().unwrap_or(bytes.len());
        bytes.get(start..end)
    }

    /// Borrow this UTF-8 string as validated text.
    pub fn as_utf8(self) -> Option<&'a str> {
        if self.kind().ok()? != PackedValueKind::Utf8 {
            return None;
        }
        str::from_utf8(self.as_sequence_bytes()?).ok()
    }

    /// Borrow the trait metadata that an owned callable would carry.
    fn callable_trait(self) -> Option<&'a TraitIdentifier> {
        match self.expected {
            TypeSignature::CallableType(CallableSubtype::Trait(identifier))
            | TypeSignature::TraitReferenceType(identifier) => Some(identifier),
            _ => None,
        }
    }

    /// Borrow the address and name components of a principal or callable contract.
    pub fn as_principal(self) -> Result<Option<PackedPrincipalView<'a>>, PackedValueError> {
        match (self.expected, self.body) {
            (
                TypeSignature::PrincipalType
                | TypeSignature::CallableType(_)
                | TypeSignature::TraitReferenceType(_),
                ViewBody::Encoded(bytes),
            ) => primitive::PackedPrincipal::from_admitted(bytes)
                .map(PackedPrincipalView::from_packed)
                .map(Some),
            _ => Ok(None),
        }
    }

    /// Project the active child of an optional, or `None` for Clarity `none`.
    pub fn optional_child(self) -> Result<Option<Self>, PackedValueError> {
        if let ViewBody::Some(child) = self.body {
            return Ok(Some(child.as_view()));
        }
        let TypeSignature::OptionalType(expected) = self.expected else {
            return Err(PackedValueError::BorrowedView("type mismatch"));
        };
        let ViewBody::Encoded(bytes) = self.body else {
            return Err(PackedValueError::BorrowedView("type mismatch"));
        };
        let (tag, child) = primitive::split_tag(bytes)?;
        match tag {
            0 if child.is_empty() => Ok(None),
            1 => Self::admitted_child(child, expected, self.epoch).map(Some),
            _ => Err(PackedValueError::BorrowedView("invalid optional")),
        }
    }

    /// Project the active response branch and report whether it is `ok`.
    pub fn response_child(self) -> Result<(bool, Self), PackedValueError> {
        if let ViewBody::Composite(CompositeValue::Response(committed, child)) = self.body {
            return Ok((*committed, child.as_view()));
        }
        let TypeSignature::ResponseType(types) = self.expected else {
            return Err(PackedValueError::BorrowedView("type mismatch"));
        };
        let ViewBody::Encoded(bytes) = self.body else {
            return Err(PackedValueError::BorrowedView("type mismatch"));
        };
        let (tag, child) = primitive::split_tag(bytes)?;
        match tag {
            0 => Self::admitted_child(child, &types.1, self.epoch).map(|child| (false, child)),
            1 => Self::admitted_child(child, &types.0, self.epoch).map(|child| (true, child)),
            _ => Err(PackedValueError::BorrowedView("invalid response")),
        }
    }

    /// Borrow this value through tuple field projection.
    pub fn as_tuple(self) -> Result<PackedTupleView<'a>, PackedValueError> {
        let TypeSignature::TupleType(expected) = self.expected else {
            return Err(PackedValueError::BorrowedView("type mismatch"));
        };
        let (bytes, fields) = match self.body {
            ViewBody::Encoded(bytes) => (bytes, None),
            ViewBody::Composite(CompositeValue::Tuple(fields)) => {
                (&[][..], Some(fields.as_slice()))
            }
            _ => return Err(PackedValueError::BorrowedView("type mismatch")),
        };
        Ok(PackedTupleView {
            bytes,
            fields,
            expected,
            epoch: self.epoch,
        })
    }

    /// Borrow this value through list element projection.
    pub fn as_list(self) -> Result<PackedListView<'a>, PackedValueError> {
        let TypeSignature::SequenceType(SequenceSubtype::ListType(expected)) = self.expected else {
            return Err(PackedValueError::BorrowedView("type mismatch"));
        };
        if let ViewBody::Composite(CompositeValue::List(list)) = self.body {
            return Ok(PackedListView {
                elements: &[],
                count: list.len(),
                expected,
                epoch: self.epoch,
                projection: None,
                composite: Some(list),
            });
        }
        if let ViewBody::List(owner) = self.body {
            let projection = owner.list_projection.as_ref().expect("list projection");
            return Ok(PackedListView {
                elements: &[],
                count: projection.selection.len(),
                expected,
                epoch: self.epoch,
                projection: Some(owner),
                composite: None,
            });
        }
        let ViewBody::Encoded(bytes) = self.body else {
            return Err(PackedValueError::BorrowedView("type mismatch"));
        };
        let (count, elements) = primitive::split_list(bytes)?;
        Ok(PackedListView {
            elements,
            projection: None,
            composite: None,
            count,
            expected,
            epoch: self.epoch,
        })
    }

    /// Materialize this borrowed value in the existing owned representation.
    pub fn to_owned_value(self) -> Result<Value, PackedValueError> {
        match self.body {
            ViewBody::Composite(value @ CompositeValue::Bytes { .. }) => {
                let bytes = value.sequence_bytes().expect("byte sequence").to_vec();
                match self.kind()? {
                    PackedValueKind::Buffer => Value::buff_from(bytes),
                    PackedValueKind::Ascii => Value::string_ascii_from_bytes(bytes),
                    PackedValueKind::Utf8 => Value::string_utf8_from_bytes(bytes),
                    _ => unreachable!("byte kind"),
                }
                .map_err(Into::into)
            }
            ViewBody::Composite(CompositeValue::Response(committed, child)) => {
                let value = child.as_view().to_owned_value()?;
                if *committed {
                    Value::okay(value)
                } else {
                    Value::error(value)
                }
                .map_err(Into::into)
            }
            ViewBody::Composite(CompositeValue::Tuple(fields)) => {
                let TypeSignature::TupleType(schema) = self.expected else {
                    unreachable!("tuple schema")
                };
                let data = fields
                    .iter()
                    .map(|(name, child)| {
                        child
                            .as_view()
                            .to_owned_value()
                            .map(|value| (name.clone(), value))
                    })
                    .collect::<Result<_, PackedValueError>>()?;
                Ok(Value::Tuple(crate::types::TupleData::new(
                    schema.clone(),
                    data,
                )))
            }
            ViewBody::List(_) | ViewBody::Composite(CompositeValue::List(_)) => {
                let list = self.as_list()?;
                let data = (0..list.len())
                    .map(|index| {
                        list.get(index)?
                            .ok_or(PackedValueError::BorrowedView("missing child"))?
                            .to_owned_value()
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(Value::Sequence(SequenceData::List(
                    crate::types::ListData {
                        data,
                        type_signature: list.expected.clone(),
                    },
                )))
            }

            ViewBody::Encoded(bytes) => {
                decode::body(bytes, self.expected).and_then(|(value, length)| {
                    if self
                        .consensus_byte_len
                        .is_none_or(|expected| length == expected)
                    {
                        Ok(value)
                    } else {
                        Err(PackedValueError::BorrowedView(
                            "canonical logical consensus length mismatch",
                        ))
                    }
                })
            }
            ViewBody::UInt(value) => Ok(Value::UInt(value)),
            ViewBody::Int(value) => Ok(Value::Int(value)),
            ViewBody::Bool(value) => Ok(Value::Bool(value)),
            ViewBody::Some(child) => {
                Value::some(child.as_view().to_owned_value()?).map_err(Into::into)
            }
        }
    }

    /// Compare two validated views by logical Clarity value without allocating owned trees.
    pub fn value_eq(self, other: Self) -> Result<bool, PackedValueError> {
        let left_kind = self.kind()?;
        if left_kind != other.kind()? {
            return Ok(false);
        }
        Ok(match left_kind {
            PackedValueKind::Int => self.as_int() == other.as_int(),
            PackedValueKind::UInt => self.as_uint() == other.as_uint(),
            PackedValueKind::Bool => self.as_bool() == other.as_bool(),
            PackedValueKind::Buffer | PackedValueKind::Ascii | PackedValueKind::Utf8 => {
                self.as_sequence_bytes() == other.as_sequence_bytes()
            }
            PackedValueKind::Principal => self.encoded_body() == other.encoded_body(),
            PackedValueKind::Callable => {
                self.encoded_body() == other.encoded_body()
                    && self.callable_trait() == other.callable_trait()
            }
            PackedValueKind::Optional => match (self.optional_child()?, other.optional_child()?) {
                (None, None) => true,
                (Some(left), Some(right)) => left.value_eq(right)?,
                (None, Some(_)) | (Some(_), None) => false,
            },
            PackedValueKind::Response => {
                let (left_committed, left) = self.response_child()?;
                let (right_committed, right) = other.response_child()?;
                left_committed == right_committed && left.value_eq(right)?
            }
            PackedValueKind::Tuple => {
                let left = self.as_tuple()?;
                let right = other.as_tuple()?;
                if left.len() != right.len() {
                    false
                } else {
                    let mut equal = true;
                    for index in 0..left.len() {
                        let Some((left_name, left_child)) = left.get_index(index)? else {
                            return Err(PackedValueError::BorrowedView(
                                "validated tuple field is missing",
                            ));
                        };
                        let Some((right_name, right_child)) = right.get_index(index)? else {
                            return Err(PackedValueError::BorrowedView(
                                "validated tuple field is missing",
                            ));
                        };
                        if left_name != right_name || !left_child.value_eq(right_child)? {
                            equal = false;
                            break;
                        }
                    }
                    equal
                }
            }
            PackedValueKind::List => {
                let left = self.as_list()?;
                let right = other.as_list()?;
                if left.len() != right.len() {
                    false
                } else {
                    let mut equal = true;
                    for index in 0..left.len() {
                        let left_child = left.get(index)?.ok_or(PackedValueError::BorrowedView(
                            "validated list element is missing",
                        ))?;
                        let right_child = right.get(index)?.ok_or(
                            PackedValueError::BorrowedView("validated list element is missing"),
                        )?;
                        if !left_child.value_eq(right_child)? {
                            equal = false;
                            break;
                        }
                    }
                    equal
                }
            }
        })
    }

    /// Compare this view with an owned Clarity value without materializing the packed tree.
    pub fn value_eq_owned(self, other: &Value) -> Result<bool, PackedValueError> {
        Ok(match (self.kind()?, other) {
            (PackedValueKind::Int, Value::Int(value)) => self.as_int() == Some(*value),
            (PackedValueKind::UInt, Value::UInt(value)) => self.as_uint() == Some(*value),
            (PackedValueKind::Bool, Value::Bool(value)) => self.as_bool() == Some(*value),
            (PackedValueKind::Buffer, Value::Sequence(SequenceData::Buffer(value))) => {
                self.as_sequence_bytes() == Some(value.data.as_slice())
            }
            (
                PackedValueKind::Ascii,
                Value::Sequence(SequenceData::String(CharType::ASCII(value))),
            ) => self.as_sequence_bytes() == Some(value.data.as_slice()),
            (
                PackedValueKind::Utf8,
                Value::Sequence(SequenceData::String(CharType::UTF8(value))),
            ) => self.as_sequence_bytes().is_some_and(|bytes| {
                value
                    .data
                    .iter()
                    .flatten()
                    .copied()
                    .eq(bytes.iter().copied())
            }),
            (PackedValueKind::Principal, Value::Principal(principal)) => {
                let bytes = self
                    .encoded_body()
                    .ok_or(PackedValueError::BorrowedView("type mismatch"))?;
                match principal {
                    PrincipalData::Standard(principal) => {
                        bytes.first() == Some(&0)
                            && standard_principal_eq(bytes.get(1..).unwrap_or_default(), principal)
                    }
                    PrincipalData::Contract(contract) => contract_principal_eq(bytes, contract),
                }
            }
            (PackedValueKind::Callable, Value::CallableContract(callable)) => {
                self.encoded_body().is_some_and(|bytes| {
                    contract_principal_eq(bytes, &callable.contract_identifier)
                }) && self.callable_trait() == callable.trait_identifier.as_deref()
            }
            (PackedValueKind::Optional, Value::Optional(value)) => {
                match (self.optional_child()?, value.data.as_deref()) {
                    (None, None) => true,
                    (Some(left), Some(right)) => left.value_eq_owned(right)?,
                    (None, Some(_)) | (Some(_), None) => false,
                }
            }
            (PackedValueKind::Response, Value::Response(value)) => {
                let (committed, child) = self.response_child()?;
                committed == value.committed && child.value_eq_owned(value.data.as_ref())?
            }
            (PackedValueKind::Tuple, Value::Tuple(value)) => {
                let packed = self.as_tuple()?;
                if packed.len() != value.data_map.len() {
                    false
                } else {
                    let mut equal = true;
                    for (index, (right_name, right_child)) in value.data_map.iter().enumerate() {
                        let Some((left_name, left_child)) = packed.get_index(index)? else {
                            return Err(PackedValueError::BorrowedView(
                                "validated tuple field is missing",
                            ));
                        };
                        if left_name != right_name || !left_child.value_eq_owned(right_child)? {
                            equal = false;
                            break;
                        }
                    }
                    equal
                }
            }
            (PackedValueKind::List, Value::Sequence(SequenceData::List(value))) => {
                let packed = self.as_list()?;
                if packed.len() != value.data.len() {
                    false
                } else {
                    let mut equal = true;
                    for (index, right_child) in value.data.iter().enumerate() {
                        let left_child = packed.get(index)?.ok_or(
                            PackedValueError::BorrowedView("validated list element is missing"),
                        )?;
                        if !left_child.value_eq_owned(right_child)? {
                            equal = false;
                            break;
                        }
                    }
                    equal
                }
            }
            _ => false,
        })
    }

    /// Borrow a child whose contents were admitted with the parent.
    fn admitted_child(
        bytes: &'a [u8],
        expected: &'a TypeSignature,
        epoch: &'a StacksEpochId,
    ) -> Result<Self, PackedValueError> {
        Ok(Self {
            body: ViewBody::Encoded(bytes),
            expected,
            epoch,
            consensus_byte_len: None,
        })
    }

    /// Construct a scalar projected from a homogeneous list lane.
    fn lane_scalar(
        body: ViewBody<'a>,
        expected: &'a TypeSignature,
        epoch: &'a StacksEpochId,
    ) -> Self {
        Self {
            body,
            expected,
            epoch,
            consensus_byte_len: Some(match body {
                ViewBody::UInt(_) | ViewBody::Int(_) => 17,
                ViewBody::Bool(_) => 1,
                ViewBody::Encoded(_)
                | ViewBody::Some(_)
                | ViewBody::List(_)
                | ViewBody::Composite(_) => {
                    unreachable!("lane scalar is decoded")
                }
            }),
        }
    }
}

/// A validated borrowed tuple body and its declared field map.
#[derive(Clone, Copy, Debug)]
pub struct PackedTupleView<'a> {
    /// Packed tuple body.
    bytes: &'a [u8],
    /// Independently shared runtime fields when no contiguous body exists.
    fields: Option<&'a [(ClarityName, SharedPackedValue)]>,
    /// Declared tuple schema in canonical field order.
    expected: &'a crate::types::TupleTypeSignature,
    /// Epoch used for child materialization.
    epoch: &'a StacksEpochId,
}

impl<'a> PackedTupleView<'a> {
    /// Return the tuple's number of fields.
    pub fn len(self) -> usize {
        if let Some(fields) = self.fields {
            return fields.len();
        }
        self.expected.get_type_map().len()
    }

    /// Return whether this tuple has no fields.
    pub fn is_empty(self) -> bool {
        self.len() == 0
    }

    /// Project one named field without materializing the tuple.
    pub fn get(self, name: &str) -> Result<Option<PackedValueView<'a>>, PackedValueError> {
        if let Some(fields) = self.fields {
            return Ok(fields
                .binary_search_by(|(key, _)| key.as_str().cmp(name))
                .ok()
                .map(|i| fields[i].1.as_view()));
        }
        let Some((index, field_type)) = self.expected.indexed_field(name) else {
            return Ok(None);
        };
        self.child(index, field_type).map(Some)
    }

    /// Project one field by canonical tuple order.
    pub fn get_index(
        self,
        index: usize,
    ) -> Result<Option<(&'a ClarityName, PackedValueView<'a>)>, PackedValueError> {
        if let Some(fields) = self.fields {
            return Ok(fields
                .get(index)
                .map(|(name, value)| (name, value.as_view())));
        }
        let Some((name, field_type)) = self.expected.field_at(index) else {
            return Ok(None);
        };
        self.child(index, field_type)
            .map(|child| Some((name, child)))
    }

    /// Address one admitted tuple child through its cached layout.
    fn child(
        self,
        index: usize,
        field_type: &'a TypeSignature,
    ) -> Result<PackedValueView<'a>, PackedValueError> {
        if let Some(fields) = self.fields {
            return Ok(fields[index].1.as_view());
        }
        let child = if let Some(range) = self.expected.packed_field_range(index) {
            self.bytes
                .get(range)
                .ok_or(PackedValueError::BorrowedView("truncated canonical tuple"))?
        } else {
            directory::Directory::parse(self.bytes, self.len())?.child(index)?
        };
        PackedValueView::admitted_child(child, field_type, self.epoch)
    }
}

/// A validated borrowed list element region and its declared element type.
#[derive(Clone, Copy, Debug)]
pub struct PackedListView<'a> {
    /// Packed element region after the count prefix.
    elements: &'a [u8],
    /// Optional selection over another retained list.
    projection: Option<&'a SharedPackedValue>,
    /// Mixed runtime elements in a balanced persistent tree.
    composite: Option<&'a SharedList>,
    /// Number of logical list elements.
    count: usize,
    /// Declared list schema.
    expected: &'a ListTypeData,
    /// Epoch used for child materialization.
    epoch: &'a StacksEpochId,
}

impl<'a> PackedListView<'a> {
    /// Return the number of list elements.
    pub const fn len(self) -> usize {
        self.count
    }

    /// Return whether the list contains no elements.
    pub const fn is_empty(self) -> bool {
        self.count == 0
    }

    /// Project one list element without materializing the list.
    pub fn get(self, index: usize) -> Result<Option<PackedValueView<'a>>, PackedValueError> {
        if index >= self.count {
            return Ok(None);
        }
        if let Some(list) = self.composite {
            return list.view(index);
        }
        if let Some(owner) = self.projection {
            let projection = owner.list_projection.as_ref().expect("list projection");
            let source_index = projection
                .selection
                .source_index(index)
                .expect("checked index");
            return projection.source.as_view().as_list()?.get(source_index);
        }
        let expected = self.expected.get_list_item_type();
        let view = match expected {
            TypeSignature::UIntType => {
                let lane = primitive::IntegerLane::from_admitted(self.elements, self.count)?;
                PackedValueView::lane_scalar(
                    ViewBody::UInt(lane.unsigned_at(index)?),
                    expected,
                    self.epoch,
                )
            }
            TypeSignature::IntType => {
                let lane = primitive::IntegerLane::from_admitted(self.elements, self.count)?;
                PackedValueView::lane_scalar(
                    ViewBody::Int(lane.signed_at(index)?),
                    expected,
                    self.epoch,
                )
            }
            TypeSignature::BoolType => {
                let byte = self
                    .elements
                    .get(index / 8)
                    .ok_or(PackedValueError::BorrowedView("truncated boolean lane"))?;
                PackedValueView::lane_scalar(
                    ViewBody::Bool(byte & (1 << (index % 8)) != 0),
                    expected,
                    self.epoch,
                )
            }
            _ => {
                let child = if let Some(width) = layout::fixed_type_width(expected)? {
                    let start = index
                        .checked_mul(width)
                        .ok_or(PackedValueError::SizeOverflow)?;
                    let end = start
                        .checked_add(width)
                        .ok_or(PackedValueError::SizeOverflow)?;
                    self.elements
                        .get(start..end)
                        .ok_or(PackedValueError::BorrowedView(
                            "truncated canonical fixed list",
                        ))?
                } else {
                    directory::Directory::parse(self.elements, self.count)?.child(index)?
                };
                PackedValueView::admitted_child(child, expected, self.epoch)?
            }
        };
        Ok(Some(view))
    }
}

/// Compare the borrowed wire identity with an owned standard principal.
fn standard_principal_eq(bytes: &[u8], principal: &StandardPrincipalData) -> bool {
    bytes.len() == 21 && bytes[0] == principal.version() && bytes[1..] == principal.1
}

/// Compare contract identity bytes without allocating or validating a contract name again.
fn contract_principal_eq(bytes: &[u8], contract: &QualifiedContractIdentifier) -> bool {
    bytes.first() == Some(&1)
        && bytes
            .get(1..22)
            .is_some_and(|issuer| standard_principal_eq(issuer, &contract.issuer))
        && bytes.get(22..) == Some(contract.name.as_bytes())
}

/// Validate a record before admitting untrusted or transient bytes.
fn validate_body(
    bytes: &[u8],
    expected: &TypeSignature,
    epoch: &StacksEpochId,
) -> Result<u32, PackedValueError> {
    inspect_body::<true>(bytes, expected, epoch)
}

/// Measure consensus framing without revisiting admitted payload contents.
fn measure_body(
    bytes: &[u8],
    expected: &TypeSignature,
    epoch: &StacksEpochId,
) -> Result<u32, PackedValueError> {
    inspect_body::<false>(bytes, expected, epoch)
}

/// Validate one packed body without constructing the owned recursive value tree.
fn inspect_body<const VALIDATE: bool>(
    bytes: &[u8],
    expected: &TypeSignature,
    epoch: &StacksEpochId,
) -> Result<u32, PackedValueError> {
    use TypeSignature::*;

    match expected {
        IntType if VALIDATE => primitive::decode_canonical_i128(bytes).map(|_| 17),
        IntType => Ok(17),
        UIntType if VALIDATE => primitive::decode_canonical_u128(bytes).map(|_| 17),
        UIntType => Ok(17),
        BoolType if VALIDATE => match bytes {
            [0] | [1] => Ok(1),
            _ => Err(PackedValueError::BorrowedView("invalid boolean")),
        },
        BoolType => Ok(1),
        SequenceType(SequenceSubtype::BufferType(max_len)) => {
            if VALIDATE && bytes.len() > u32::from(max_len) as usize {
                return Err(PackedValueError::BorrowedView(
                    "buffer exceeds declared bound",
                ));
            }
            primitive::logical_sequence_len(bytes.len())
        }
        SequenceType(SequenceSubtype::StringType(StringSubtype::ASCII(max_len))) => {
            if VALIDATE
                && (bytes.len() > u32::from(max_len) as usize
                    || !bytes.iter().all(primitive::valid_ascii_byte))
            {
                return Err(PackedValueError::BorrowedView("invalid ASCII string"));
            }
            primitive::logical_sequence_len(bytes.len())
        }
        SequenceType(SequenceSubtype::StringType(StringSubtype::UTF8(max_len))) => {
            if !VALIDATE {
                return primitive::logical_sequence_len(bytes.len());
            }
            let string = str::from_utf8(bytes)
                .map_err(|_| PackedValueError::BorrowedView("invalid UTF-8 string"))?;
            if string.chars().count() > u32::from(max_len) as usize {
                return Err(PackedValueError::BorrowedView(
                    "UTF-8 string exceeds declared bound",
                ));
            }
            primitive::logical_sequence_len(bytes.len())
        }
        PrincipalType | CallableType(_) | TraitReferenceType(_) if !VALIDATE => {
            match bytes.first() {
                Some(0) => Ok(22),
                Some(1) => primitive::checked_logical_add(
                    1,
                    u32::try_from(bytes.len()).map_err(|_| PackedValueError::SizeOverflow)?,
                ),
                _ => Err(PackedValueError::BorrowedView("invalid principal kind")),
            }
        }
        PrincipalType => primitive::PackedPrincipal::parse(bytes)?.consensus_byte_len(),
        CallableType(subtype) => validate_callable(bytes, Some(subtype)),
        TraitReferenceType(_) => validate_callable(bytes, None),
        OptionalType(inner) => {
            let (tag, child) = primitive::split_tag(bytes)?;
            match tag {
                0 if child.is_empty() => Ok(1),
                1 => primitive::checked_logical_add(
                    1,
                    inspect_body::<VALIDATE>(child, inner, epoch)?,
                ),
                _ => Err(PackedValueError::BorrowedView("invalid optional")),
            }
        }
        ResponseType(types) => {
            let (tag, child) = primitive::split_tag(bytes)?;
            let child_type = match tag {
                0 => &types.1,
                1 => &types.0,
                _ => return Err(PackedValueError::BorrowedView("invalid response")),
            };
            primitive::checked_logical_add(1, inspect_body::<VALIDATE>(child, child_type, epoch)?)
        }
        TupleType(tuple) => validate_tuple::<VALIDATE>(bytes, tuple, epoch),
        SequenceType(SequenceSubtype::ListType(list)) => {
            validate_list::<VALIDATE>(bytes, list, epoch)
        }
        NoType => Err(PackedValueError::BorrowedView("NoType cannot be active")),
        ListUnionType(_) => Err(PackedValueError::BorrowedView(
            "ListUnionType is analysis-only",
        )),
    }
}

/// Validate a callable principal without allocating its owned contract identifier.
fn validate_callable(
    bytes: &[u8],
    expected: Option<&CallableSubtype>,
) -> Result<u32, PackedValueError> {
    let principal = primitive::PackedPrincipal::parse(bytes)?;
    let primitive::PackedPrincipal::Contract { issuer, name } = &principal else {
        return Err(PackedValueError::BorrowedView(
            "callable must contain a contract principal",
        ));
    };
    if let Some(CallableSubtype::Principal(contract)) = expected
        && (issuer[0] != contract.issuer.version()
            || issuer[1..] != contract.issuer.1
            || *name != contract.name.as_str())
    {
        return Err(PackedValueError::BorrowedView("type mismatch"));
    }
    principal.consensus_byte_len()
}

/// Validate tuple framing and return its consensus-serialized length.
fn validate_tuple<const VALIDATE: bool>(
    bytes: &[u8],
    expected: &crate::types::TupleTypeSignature,
    epoch: &StacksEpochId,
) -> Result<u32, PackedValueError> {
    let all_fixed = expected
        .get_type_map()
        .values()
        .try_fold(true, |all_fixed, child| {
            Ok::<_, PackedValueError>(all_fixed && layout::fixed_type_width(child)?.is_some())
        })?;
    let mut logical_len = 5u32;
    if all_fixed {
        let mut cursor = 0usize;
        for (name, field_type) in expected.get_type_map() {
            let width = layout::fixed_type_width(field_type)?.ok_or(
                PackedValueError::BorrowedView("canonical fixed tuple classification changed"),
            )?;
            let end = cursor
                .checked_add(width)
                .ok_or(PackedValueError::SizeOverflow)?;
            let child = bytes
                .get(cursor..end)
                .ok_or(PackedValueError::BorrowedView("truncated canonical tuple"))?;
            logical_len = primitive::tuple_logical_add(
                logical_len,
                name.as_str(),
                inspect_body::<VALIDATE>(child, field_type, epoch)?,
            )?;
            cursor = end;
        }
        if cursor != bytes.len() {
            return Err(PackedValueError::BorrowedView(
                "canonical fixed tuple has trailing bytes",
            ));
        }
    } else {
        let directory = directory::Directory::parse(bytes, expected.get_type_map().len())?;
        for ((name, field_type), child) in expected.get_type_map().iter().zip(directory.children())
        {
            logical_len = primitive::tuple_logical_add(
                logical_len,
                name.as_str(),
                inspect_body::<VALIDATE>(child?, field_type, epoch)?,
            )?;
        }
    }
    Ok(logical_len)
}

/// Validate list framing and return its consensus-serialized length.
fn validate_list<const VALIDATE: bool>(
    bytes: &[u8],
    expected: &ListTypeData,
    epoch: &StacksEpochId,
) -> Result<u32, PackedValueError> {
    let (count, elements) = primitive::split_list(bytes)?;
    if count > expected.get_max_len() as usize {
        return Err(PackedValueError::BorrowedView(
            "list exceeds declared bound",
        ));
    }
    if count == 0 {
        return if elements.is_empty() {
            Ok(5)
        } else {
            Err(PackedValueError::BorrowedView(
                "empty list has an element region",
            ))
        };
    }
    let element_type = expected.get_list_item_type();
    let children_len = match element_type {
        TypeSignature::UIntType if VALIDATE => {
            primitive::IntegerLane::parse_unsigned(elements, count)?.consensus_byte_len()?
        }
        TypeSignature::IntType if VALIDATE => {
            primitive::IntegerLane::parse_signed(elements, count)?.consensus_byte_len()?
        }
        TypeSignature::BoolType if VALIDATE => primitive::validate_bool_lane(elements, count)?,
        TypeSignature::UIntType | TypeSignature::IntType => u32::try_from(count)
            .ok()
            .and_then(|n| n.checked_mul(17))
            .ok_or(PackedValueError::SizeOverflow)?,
        TypeSignature::BoolType => {
            u32::try_from(count).map_err(|_| PackedValueError::SizeOverflow)?
        }
        _ => match layout::fixed_type_width(element_type)? {
            Some(width) => {
                if elements.len()
                    != count
                        .checked_mul(width)
                        .ok_or(PackedValueError::SizeOverflow)?
                {
                    return Err(PackedValueError::BorrowedView(
                        "canonical fixed list byte length mismatch",
                    ));
                }
                let mut logical_len = 0u32;
                for child in elements.chunks_exact(width) {
                    logical_len = primitive::checked_logical_add(
                        logical_len,
                        inspect_body::<VALIDATE>(child, element_type, epoch)?,
                    )?;
                }
                logical_len
            }
            None => {
                let directory = directory::Directory::parse(elements, count)?;
                let mut logical_len = 0u32;
                for child in directory.children() {
                    logical_len = primitive::checked_logical_add(
                        logical_len,
                        inspect_body::<VALIDATE>(child?, element_type, epoch)?,
                    )?;
                }
                logical_len
            }
        },
    };
    primitive::checked_logical_add(5, children_len)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use stacks_common::types::StacksEpochId;

    use super::{PackedPrincipalView, PackedValueKind, PackedValueView};
    use crate::representations::ClarityName;
    use crate::types::codec::packed::{PackedValue, PackedValueVersion, SharedPackedValue};
    use crate::types::signatures::{BufferLength, SequenceSubtype};
    use crate::types::{
        CallableData, ListTypeData, PrincipalData, QualifiedContractIdentifier, TupleData,
        TypeSignature, Value,
    };

    /// Encode one value and expose it through the borrowed-view parser.
    fn view<'a>(
        value: &Value,
        expected: &'a TypeSignature,
        epoch: &'a StacksEpochId,
        storage: &'a mut Vec<u8>,
    ) -> PackedValueView<'a> {
        *storage = PackedValue::encode(PackedValueVersion::V1, value)
            .unwrap()
            .into_bytes();
        PackedValueView::parse(
            crate::types::codec::packed::PackedValueRef::parse(storage).unwrap(),
            expected,
            epoch,
        )
        .unwrap()
    }

    #[test]
    fn borrowed_scalar_and_sequence_views_match_owned_values() {
        let epoch = StacksEpochId::latest();
        let cases = [
            Value::UInt(123456),
            Value::Int(-123456),
            Value::Bool(true),
            Value::buff_from(vec![1, 2, 3, 4]).unwrap(),
        ];
        for value in cases {
            let expected = TypeSignature::type_of(&value).unwrap();
            let mut storage = Vec::new();
            let borrowed = view(&value, &expected, &epoch, &mut storage);
            assert_eq!(borrowed.to_owned_value().unwrap(), value);
        }
    }

    #[test]
    fn logical_size_matches_materialization_under_wider_declared_bounds() {
        let epoch = StacksEpochId::latest();
        let wide_buffer = TypeSignature::SequenceType(SequenceSubtype::BufferType(
            BufferLength::try_from(1_024u32).unwrap(),
        ));
        let cases = [
            (
                Value::buff_from(vec![1, 2, 3]).unwrap(),
                wide_buffer.clone(),
            ),
            (
                Value::some(Value::buff_from(vec![4, 5]).unwrap()).unwrap(),
                TypeSignature::new_option(wide_buffer).unwrap(),
            ),
            (
                Value::list_from(vec![Value::UInt(1), Value::UInt(2)]).unwrap(),
                TypeSignature::SequenceType(SequenceSubtype::ListType(
                    ListTypeData::new_list(TypeSignature::UIntType, 100).unwrap(),
                )),
            ),
        ];

        for (value, expected) in cases {
            let mut storage = Vec::new();
            let borrowed = view(&value, &expected, &epoch, &mut storage);
            let materialized = borrowed.to_owned_value().unwrap();
            assert_eq!(
                borrowed.logical_size().unwrap(),
                materialized.size().unwrap(),
                "declared schema: {expected:?}"
            );
            assert_eq!(
                borrowed.logical_type().unwrap(),
                TypeSignature::type_of(&materialized).unwrap(),
                "declared schema: {expected:?}"
            );
        }
    }

    #[test]
    fn packed_to_owned_equality_covers_every_value_family() {
        let epoch = StacksEpochId::latest();
        let cases = vec![
            Value::Int(-7),
            Value::UInt(7),
            Value::Bool(true),
            Value::buff_from(vec![1, 2, 3]).unwrap(),
            Value::string_ascii_from_bytes(b"hello".to_vec()).unwrap(),
            Value::string_utf8_from_string_utf8_literal("h\\u{e9}llo".into()).unwrap(),
            Value::Principal(PrincipalData::Standard(
                QualifiedContractIdentifier::transient().issuer,
            )),
            Value::CallableContract(CallableData {
                contract_identifier: QualifiedContractIdentifier::transient(),
                trait_identifier: None,
            }),
            Value::none(),
            Value::some(Value::UInt(1)).unwrap(),
            Value::okay(Value::Bool(true)).unwrap(),
            Value::error(Value::Int(-1)).unwrap(),
            Value::Tuple(
                TupleData::from_data(vec![
                    (ClarityName::try_from("flag").unwrap(), Value::Bool(true)),
                    (ClarityName::try_from("value").unwrap(), Value::UInt(1)),
                ])
                .unwrap(),
            ),
            Value::list_from(vec![Value::UInt(1), Value::UInt(2)]).unwrap(),
        ];

        for value in cases {
            let expected = TypeSignature::type_of(&value).unwrap();
            let mut storage = Vec::new();
            let borrowed = view(&value, &expected, &epoch, &mut storage);
            assert!(borrowed.value_eq_owned(&value).unwrap(), "{value:?}");
            assert!(!borrowed.value_eq_owned(&Value::Bool(false)).unwrap());
            let owner = Arc::new(storage);
            let admitted = SharedPackedValue::from_encoded_owner(
                owner.clone(),
                0..owner.len(),
                &expected,
                &epoch,
            )
            .unwrap();
            assert!(admitted.as_view().value_eq_owned(&value).unwrap());
            assert_eq!(admitted.to_owned_value().unwrap(), value);
            assert_eq!(
                admitted.consensus_byte_len(),
                value.serialize_to_vec().unwrap().len() as u32
            );
        }
    }

    #[test]
    fn principal_views_borrow_address_and_contract_name_components() {
        let epoch = StacksEpochId::latest();
        let contract = QualifiedContractIdentifier::transient();
        let issuer_version = contract.issuer.version();
        let issuer_hash = contract.issuer.1;
        let contract_name = contract.name.to_string();
        let value = Value::Principal(PrincipalData::Contract(contract));
        let expected = TypeSignature::type_of(&value).unwrap();
        let mut storage = Vec::new();
        let borrowed = view(&value, &expected, &epoch, &mut storage);

        assert_eq!(
            borrowed.as_principal().unwrap(),
            Some(PackedPrincipalView::Contract {
                issuer_version,
                issuer_hash: &issuer_hash,
                name: &contract_name,
            })
        );
    }

    #[test]
    fn tuple_projection_borrows_only_the_selected_field() {
        let epoch = StacksEpochId::latest();
        let value = Value::Tuple(
            TupleData::from_data(vec![
                (ClarityName::try_from("flag").unwrap(), Value::Bool(true)),
                (
                    ClarityName::try_from("payload").unwrap(),
                    Value::buff_from(vec![7; 1024]).unwrap(),
                ),
            ])
            .unwrap(),
        );
        let expected = TypeSignature::type_of(&value).unwrap();
        let mut storage = Vec::new();
        let borrowed = view(&value, &expected, &epoch, &mut storage);
        assert_eq!(borrowed.kind().unwrap(), PackedValueKind::Tuple);
        let payload = borrowed
            .as_tuple()
            .unwrap()
            .get("payload")
            .unwrap()
            .unwrap();
        assert_eq!(payload.as_sequence_bytes().unwrap(), &[7; 1024]);
        assert_eq!(
            payload.to_owned_value().unwrap(),
            Value::buff_from(vec![7; 1024]).unwrap()
        );
    }

    #[test]
    fn list_projection_decodes_individual_lane_elements() {
        let epoch = StacksEpochId::latest();
        let value =
            Value::list_from(vec![Value::UInt(1), Value::UInt(255), Value::UInt(65_535)]).unwrap();
        let expected = TypeSignature::type_of(&value).unwrap();
        let mut storage = Vec::new();
        let borrowed = view(&value, &expected, &epoch, &mut storage);
        let list = borrowed.as_list().unwrap();
        assert_eq!(list.len(), 3);
        assert_eq!(list.get(0).unwrap().unwrap().as_uint(), Some(1));
        assert_eq!(list.get(1).unwrap().unwrap().as_uint(), Some(255));
        assert_eq!(list.get(2).unwrap().unwrap().as_uint(), Some(65_535));
        assert!(list.get(3).unwrap().is_none());
    }

    #[test]
    fn logical_length_mismatch_fails_before_view_exposure() {
        let epoch = StacksEpochId::latest();
        let value = Value::Bool(true);
        let expected = TypeSignature::BoolType;
        let mut storage = Vec::new();
        let borrowed = view(&value, &expected, &epoch, &mut storage);
        assert_eq!(borrowed.consensus_byte_len(), 1);
        storage[3] = 2;
        assert!(
            PackedValueView::parse(
                crate::types::codec::packed::PackedValueRef::parse(&storage).unwrap(),
                &expected,
                &epoch,
            )
            .is_err()
        );
    }

    #[test]
    fn shared_packed_value_clones_without_copying_the_record() {
        let epoch = StacksEpochId::latest();
        let value = Value::buff_from(vec![9; 4096]).unwrap();
        let expected = TypeSignature::type_of(&value).unwrap();
        let mut storage = Vec::new();
        let borrowed = view(&value, &expected, &epoch, &mut storage);
        assert_eq!(borrowed.as_sequence_bytes().unwrap(), &[9; 4096]);

        let shared =
            crate::types::codec::packed::SharedPackedValue::copy_from(&storage, &expected, &epoch)
                .unwrap();
        let cloned = shared.clone();
        assert_eq!(shared.record_owner_count(), 2);
        assert_eq!(cloned.record_owner_count(), 2);
        assert_eq!(cloned.as_view().as_sequence_bytes().unwrap(), &[9; 4096]);
        assert_eq!(shared, cloned);
        assert!(!shared.is_materialized());
        assert_eq!(cloned.to_owned_value().unwrap(), value);
    }
    /// Child lengths stay lazy while nested results retain their original bytes and semantics.
    #[test]
    fn admitted_nested_projections_match_owned_lengths() {
        use crate::types::TupleData;
        use crate::types::codec::packed::{PackedValue, PackedValueVersion, SharedPackedValue};
        use std::sync::Arc;
        let epoch = StacksEpochId::latest();
        let nested = Value::from(
            TupleData::from_data(vec![
                (
                    "text".try_into().unwrap(),
                    Value::string_ascii_from_bytes(vec![b'x'; 8192]).unwrap(),
                ),
                (
                    "items".try_into().unwrap(),
                    Value::list_from(vec![Value::UInt(1), Value::UInt(65535)]).unwrap(),
                ),
            ])
            .unwrap(),
        );
        let value = Value::some(nested.clone()).unwrap();
        let expected = TypeSignature::type_of(&value).unwrap();
        let encoded = PackedValue::encode(PackedValueVersion::V1, &value).unwrap();
        let owner = Arc::new(encoded.as_bytes().to_vec());
        let shared =
            SharedPackedValue::from_encoded_owner(owner.clone(), 0..owner.len(), &expected, &epoch)
                .unwrap();
        let tuple = shared.optional_child().unwrap().unwrap();
        assert!(tuple.as_view().known_consensus_byte_len().is_none());
        let text = tuple.tuple_field("text").unwrap().unwrap();
        assert!(text.as_view().known_consensus_byte_len().is_none());
        assert_eq!(text.consensus_byte_len(), 8197);
        assert_eq!(text.as_view().known_consensus_byte_len(), Some(8197));
        let text_ptr = text.as_view().as_sequence_bytes().unwrap().as_ptr();
        assert!(
            (owner.as_ptr() as usize..owner.as_ptr() as usize + owner.len())
                .contains(&(text_ptr as usize))
        );
        assert_eq!(tuple.to_owned_value().unwrap(), nested);
        assert_eq!(
            tuple.consensus_byte_len(),
            value.serialize_to_vec().unwrap().len() as u32 - 1
        );
        let items = tuple.tuple_field("items").unwrap().unwrap();
        assert_eq!(items.consensus_byte_len(), 39);
        assert!(!shared.is_materialized());
        assert!(!tuple.is_materialized());
        drop(shared);
        drop(tuple);
        drop(owner);
        assert_eq!(text.as_view().as_sequence_bytes().unwrap().len(), 8192);
    }

    /// General admission must still reject malformed content and incompatible declared bounds.
    #[test]
    fn general_admission_rejects_bad_content_and_bounds() {
        use crate::types::codec::packed::{PackedValue, PackedValueVersion, SharedPackedValue};
        let value = Value::string_ascii_from_bytes(vec![b'x'; 8]).unwrap();
        let expected = TypeSignature::type_of(&value).unwrap();
        let encoded = PackedValue::encode(PackedValueVersion::V1, &value).unwrap();
        let mut bytes = encoded.as_bytes().to_vec();
        *bytes.last_mut().unwrap() = 255;
        assert!(SharedPackedValue::copy_from(&bytes, &expected, &StacksEpochId::latest()).is_err());
        let narrow =
            TypeSignature::type_of(&Value::string_ascii_from_bytes(vec![b'x']).unwrap()).unwrap();
        assert!(
            SharedPackedValue::copy_from(encoded.as_bytes(), &narrow, &StacksEpochId::latest())
                .is_err()
        );
    }

    /// Materialized compatibility values remain reusable when a populated handle is cloned.
    #[test]
    fn populated_materialization_cache_clones_cheaply() {
        use crate::types::codec::packed::{PackedValue, PackedValueVersion, SharedPackedValue};
        use std::ptr;
        let value = Value::buff_from(vec![1; 1024]).unwrap();
        let expected = TypeSignature::type_of(&value).unwrap();
        let encoded = PackedValue::encode(PackedValueVersion::V1, &value).unwrap();
        let shared =
            SharedPackedValue::copy_from(encoded.as_bytes(), &expected, &StacksEpochId::latest())
                .unwrap();
        let first = shared.materialized().unwrap();
        let clone = shared.clone();
        assert!(ptr::eq(first, clone.materialized().unwrap()));
    }
}
