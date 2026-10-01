//! Runtime composites retaining independently owned or mapped child values.

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::sync::{Arc, OnceLock};

use stacks_common::types::StacksEpochId;

use super::{
    PackedValue, PackedValueError, PackedValueKind, PackedValueVersion, PackedValueView,
    SharedPackedValue,
};
use crate::errors::ClarityTypeError;
use crate::representations::ClarityName;
use crate::types::signatures::CallableSubtype;
use crate::types::{
    BufferLength, CallableData, CharType, ListTypeData, PrincipalData, SequenceData,
    SequenceSubtype, StringSubtype, StringUTF8Length, TupleTypeSignature, TypeSignature, Value,
};

/// Shared runtime structure; payload bytes remain in each child's original owner.
#[derive(Clone, Debug)]
pub enum CompositeValue {
    /// Persistent balanced list with shared sequence regions and owned child handles.
    List(SharedList),
    /// Segmented byte sequence, coalesced only for consumers requiring one contiguous slice.
    Bytes {
        tree: SharedList,
        contiguous: OnceLock<Vec<u8>>,
    },
    /// Canonically ordered tuple fields.
    Tuple(Vec<(ClarityName, SharedPackedValue)>),
    /// Response branch and its retained child.
    Response(bool, SharedPackedValue),
}

/// A persistent sequence tree with logarithmic append and indexed access.
#[derive(Clone, Debug)]
pub struct SharedList(Arc<ListNode>);

/// Cached shape and immutable list storage.
#[derive(Debug)]
struct ListNode {
    /// Number of logical elements.
    len: usize,
    /// Physical byte length for segmented byte sequences; zero for list-item leaves.
    byte_len: usize,
    /// AVL height; empty leaves have height one.
    height: u8,
    /// Immutable children or a view of an existing list.
    body: ListBody,
}

/// Leaves avoid copying payloads; branches bound repeated append traversal.
#[derive(Debug)]
enum ListBody {
    /// A retained packed or projected list.
    Source(SharedPackedValue),
    /// A small group of independently shared items.
    Items(Vec<SharedPackedValue>),
    /// Two balanced subsequences.
    Branch(SharedList, SharedList),
}

impl SharedList {
    /// Retain a source region, reusing an existing sequence tree when available.
    pub fn source(value: SharedPackedValue) -> Result<Self, PackedValueError> {
        if let Some(composite) = &value.composite {
            if let CompositeValue::List(list) | CompositeValue::Bytes { tree: list, .. } =
                composite.as_ref()
            {
                return Ok(list.clone());
            }
        }
        let (len, byte_len) = if let Some(bytes) = value.as_view().as_sequence_bytes() {
            (value.utf8_len().unwrap_or(bytes.len()), bytes.len())
        } else {
            (value.as_view().as_list()?.len(), 0)
        };
        Ok(Self(Arc::new(ListNode {
            len,
            byte_len,
            height: 1,
            body: ListBody::Source(value),
        })))
    }

    /// Build bounded leaves from child handles without copying their payloads.
    pub fn items(mut items: Vec<SharedPackedValue>) -> Self {
        if items.len() <= 32 {
            return Self(Arc::new(ListNode {
                len: items.len(),
                byte_len: 0,
                height: 1,
                body: ListBody::Items(items),
            }));
        }
        let right = items.split_off(items.len() / 2);
        Self::branch(Self::items(items), Self::items(right))
    }

    /// Number of items in the complete tree.
    pub fn len(&self) -> usize {
        self.0.len
    }

    /// Construct a branch with cached shape.
    fn branch(left: Self, right: Self) -> Self {
        Self(Arc::new(ListNode {
            len: left.len() + right.len(),
            byte_len: left.0.byte_len + right.0.byte_len,
            height: 1 + left.0.height.max(right.0.height),
            body: ListBody::Branch(left, right),
        }))
    }

    /// Rebalance a joined pair whose child heights differ by at most two.
    fn balance(left: Self, right: Self) -> Self {
        if left.0.height > right.0.height + 1 {
            let ListBody::Branch(a, b) = &left.0.body else {
                unreachable!("tall leaf")
            };
            if a.0.height >= b.0.height {
                return Self::branch(a.clone(), Self::branch(b.clone(), right));
            }
            let ListBody::Branch(b1, b2) = &b.0.body else {
                unreachable!("tall leaf")
            };
            return Self::branch(
                Self::branch(a.clone(), b1.clone()),
                Self::branch(b2.clone(), right),
            );
        }
        if right.0.height > left.0.height + 1 {
            let ListBody::Branch(a, b) = &right.0.body else {
                unreachable!("tall leaf")
            };
            if b.0.height >= a.0.height {
                return Self::branch(Self::branch(left, a.clone()), b.clone());
            }
            let ListBody::Branch(a1, a2) = &a.0.body else {
                unreachable!("tall leaf")
            };
            return Self::branch(
                Self::branch(left, a1.clone()),
                Self::branch(a2.clone(), b.clone()),
            );
        }
        Self::branch(left, right)
    }

    /// Concatenate immutable trees without growing a linear chain.
    pub fn concat(left: Self, right: Self) -> Self {
        if left.len() == 0 {
            return right;
        }
        if right.len() == 0 {
            return left;
        }
        if let (ListBody::Items(a), ListBody::Items(b)) = (&left.0.body, &right.0.body) {
            if a.len() + b.len() <= 32 {
                let mut items = a.clone();
                items.extend(b.iter().cloned());
                return Self::items(items);
            }
        }
        if left.0.height > right.0.height + 1 {
            let ListBody::Branch(a, b) = &left.0.body else {
                unreachable!("tall leaf")
            };
            return Self::balance(a.clone(), Self::concat(b.clone(), right));
        }
        if right.0.height > left.0.height + 1 {
            let ListBody::Branch(a, b) = &right.0.body else {
                unreachable!("tall leaf")
            };
            return Self::balance(Self::concat(left, a.clone()), b.clone());
        }
        Self::branch(left, right)
    }

    /// Borrow an element without constructing a retained handle.
    pub fn view(&self, index: usize) -> Result<Option<PackedValueView<'_>>, PackedValueError> {
        if index >= self.len() {
            return Ok(None);
        }
        match &self.0.body {
            ListBody::Source(source) => source.as_view().as_list()?.get(index),
            ListBody::Items(items) => Ok(Some(items[index].as_view())),
            ListBody::Branch(left, right) => {
                if index < left.len() {
                    left.view(index)
                } else {
                    right.view(index - left.len())
                }
            }
        }
    }

    /// Retain a selected child without copying its payload.
    pub fn child(&self, index: usize) -> Result<Option<SharedPackedValue>, PackedValueError> {
        if index >= self.len() {
            return Ok(None);
        }
        match &self.0.body {
            ListBody::Source(source) => source.list_child(index),
            ListBody::Items(items) => Ok(Some(items[index].clone())),
            ListBody::Branch(left, right) => {
                if index < left.len() {
                    left.child(index)
                } else {
                    right.child(index - left.len())
                }
            }
        }
    }
}

impl SharedPackedValue {
    /// Construct a runtime composite with no contiguous packed body.
    fn from_composite(
        value: CompositeValue,
        expected: TypeSignature,
        epoch: StacksEpochId,
    ) -> Self {
        stacks_profiler::diagnostics::count("composite_values_created", 1);
        Self {
            bytes: Arc::new(Vec::<u8>::new()),
            body_range: 0..0,
            expected,
            epoch,
            consensus_byte_len: OnceLock::new(),
            materialized: OnceLock::new(),
            utf8_codepoints: OnceLock::new(),
            optional_child: None,
            list_projection: None,
            composite: Some(Arc::new(value)),
        }
    }

    /// Move an owned value into shared storage. Buffer and ASCII payload allocations are retained.
    pub fn from_value(value: Value, epoch: &StacksEpochId) -> Result<Self, PackedValueError> {
        let expected = TypeSignature::type_of(&value)?;
        match value {
            Value::Sequence(SequenceData::List(list)) => {
                let children = list
                    .data
                    .into_iter()
                    .map(|value| Self::from_value(value, epoch))
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(Self::from_composite(
                    CompositeValue::List(SharedList::items(children)),
                    expected,
                    *epoch,
                ))
            }
            Value::Tuple(tuple) => {
                let children = tuple
                    .data_map
                    .into_iter()
                    .map(|(name, value)| Self::from_value(value, epoch).map(|value| (name, value)))
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(Self::from_composite(
                    CompositeValue::Tuple(children),
                    expected,
                    *epoch,
                ))
            }
            Value::Optional(optional) if optional.data.is_some() => {
                Self::from_value(*optional.data.expect("some child"), epoch)?.into_optional()
            }
            Value::Response(response) => {
                Self::from_value(*response.data, epoch)?.into_response(response.committed)
            }
            Value::Sequence(SequenceData::Buffer(buffer)) => {
                Ok(Self::from_payload(buffer.data, expected, *epoch))
            }
            Value::Sequence(SequenceData::String(CharType::ASCII(string))) => {
                Ok(Self::from_payload(string.data, expected, *epoch))
            }
            Value::Sequence(SequenceData::String(CharType::UTF8(string))) => {
                Ok(Self::from_payload(
                    string.data.into_iter().flatten().collect(),
                    expected,
                    *epoch,
                ))
            }
            value => {
                let packed = PackedValue::encode(PackedValueVersion::V1, &value)?;
                let bytes = Arc::new(packed.into_bytes());
                let len = bytes.len();
                Self::from_encoded_owner(bytes, 0..len, &expected, epoch)
            }
        }
    }

    /// Retain an owned byte sequence directly as an admitted packed payload.
    fn from_payload(bytes: Vec<u8>, expected: TypeSignature, epoch: StacksEpochId) -> Self {
        Self {
            body_range: 0..bytes.len(),
            bytes: Arc::new(bytes),
            expected,
            epoch,
            consensus_byte_len: OnceLock::new(),
            materialized: OnceLock::new(),
            utf8_codepoints: OnceLock::new(),
            optional_child: None,
            list_projection: None,
            composite: None,
        }
    }

    /// Construct a response retaining the complete child view.
    pub fn into_response(self, committed: bool) -> Result<Self, PackedValueError> {
        let child = self.logical_type()?;
        let expected = if committed {
            TypeSignature::new_response(child, TypeSignature::NoType)?
        } else {
            TypeSignature::new_response(TypeSignature::NoType, child)?
        };
        let epoch = self.epoch;
        Ok(Self::from_composite(
            CompositeValue::Response(committed, self),
            expected,
            epoch,
        ))
    }

    /// Construct a canonical tuple retaining each field's existing owner.
    pub fn tuple(
        fields: Vec<(ClarityName, Self)>,
        epoch: &StacksEpochId,
    ) -> Result<Self, PackedValueError> {
        let mut types = BTreeMap::new();
        let mut values = BTreeMap::new();
        for (name, value) in fields {
            if types.insert(name.clone(), value.logical_type()?).is_some() {
                return Err(ClarityTypeError::DuplicateTupleField(name.into()).into());
            }
            values.insert(name, value);
        }
        let expected = TypeSignature::TupleType(TupleTypeSignature::try_from(types)?);
        Ok(Self::from_composite(
            CompositeValue::Tuple(values.into_iter().collect()),
            expected,
            *epoch,
        ))
    }

    /// Construct a list retaining child views and applying historical sanitization.
    pub fn list(items: Vec<Self>, epoch: &StacksEpochId) -> Result<Self, PackedValueError> {
        let types = items
            .iter()
            .map(Self::logical_type)
            .collect::<Result<Vec<_>, _>>()?;
        let schema = TypeSignature::parent_list_type(&types)?;
        let items = items
            .into_iter()
            .map(|value| {
                value
                    .sanitize(epoch, schema.get_list_item_type())
                    .map(|(value, _)| value)
                    .ok_or_else(|| PackedValueError::from(ClarityTypeError::ListTypeMismatch))
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self::from_composite(
            CompositeValue::List(SharedList::items(items)),
            TypeSignature::SequenceType(SequenceSubtype::ListType(schema)),
            *epoch,
        ))
    }

    /// Apply the owned sanitizer's recursive metadata rules without decoding payload bytes.
    pub fn sanitize(self, epoch: &StacksEpochId, expected: &TypeSignature) -> Option<(Self, bool)> {
        stacks_profiler::diagnostics::count("composite_schema_visits", 1);
        if !epoch.value_sanitizing() {
            return Some((self, false));
        }
        let mut changed = false;
        let result = match self.kind().ok()? {
            PackedValueKind::List => {
                let TypeSignature::SequenceType(SequenceSubtype::ListType(schema)) = expected
                else {
                    return None;
                };
                let count = self.as_view().as_list().ok()?.len();
                if count > schema.get_max_len() as usize {
                    return None;
                }
                let mut items = Vec::with_capacity(count);
                for i in 0..count {
                    let (child, did_change) = self
                        .list_child(i)
                        .ok()??
                        .sanitize(epoch, schema.get_list_item_type())?;
                    changed |= did_change;
                    items.push(child);
                }
                let types = items
                    .iter()
                    .map(Self::logical_type)
                    .collect::<Result<Vec<_>, _>>()
                    .ok()?;
                let schema = TypeSignature::parent_list_type(&types).ok()?;
                Self::from_composite(
                    CompositeValue::List(SharedList::items(items)),
                    TypeSignature::SequenceType(SequenceSubtype::ListType(schema)),
                    *epoch,
                )
            }
            PackedValueKind::Tuple => {
                let TypeSignature::TupleType(schema) = expected else {
                    return None;
                };
                let mut fields = Vec::with_capacity(schema.get_type_map().len());
                changed = self.tuple_len().ok()? != schema.get_type_map().len();
                for (name, ty) in schema.get_type_map() {
                    let (child, did_change) = self.tuple_field(name).ok()??.sanitize(epoch, ty)?;
                    changed |= did_change;
                    fields.push((name.clone(), child));
                }
                Self::tuple(fields, epoch).ok()?
            }
            PackedValueKind::Optional => {
                let TypeSignature::OptionalType(inner) = expected else {
                    return None;
                };
                if let Some(child) = self.optional_child().ok()? {
                    let (child, did_change) = child.sanitize(epoch, inner)?;
                    changed = did_change;
                    child.into_optional().ok()?
                } else {
                    return Some((self, false));
                }
            }
            PackedValueKind::Response => {
                let TypeSignature::ResponseType(types) = expected else {
                    return None;
                };
                let (committed, child) = self.response_child().ok()?;
                let (child, did_change) =
                    child.sanitize(epoch, if committed { &types.0 } else { &types.1 })?;
                changed = did_change;
                child.into_response(committed).ok()?
            }
            _ => self,
        };
        expected
            .admits_type(epoch, &result.logical_type().ok()?)
            .ok()?
            .then_some((result, changed))
    }

    /// Replace only a list's logical bound after its caller has checked the item count.
    pub fn with_list_bound(mut self, max_len: u32) -> Result<Self, PackedValueError> {
        let TypeSignature::SequenceType(SequenceSubtype::ListType(list)) = &self.expected else {
            return Err(PackedValueError::BorrowedView("expected list"));
        };
        let mut list = list.clone();
        list.reduce_max_len(max_len);
        self.expected = TypeSignature::SequenceType(SequenceSubtype::ListType(list));
        self.materialized = OnceLock::new();
        Ok(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mapped payloads and newly owned buffers retain their addresses through mixed aggregates.
    #[test]
    fn mixed_composites_retain_payloads_and_serialize_exactly() {
        let epoch = StacksEpochId::latest();
        let bytes = vec![7; 32768];
        let address = bytes.as_ptr();
        let owned =
            SharedPackedValue::from_value(Value::buff_from(bytes).unwrap(), &epoch).unwrap();
        assert_eq!(
            owned.as_view().as_sequence_bytes().unwrap().as_ptr(),
            address
        );
        let value = Value::buff_from(vec![9; 16384]).unwrap();
        let encoded = PackedValue::encode(PackedValueVersion::V1, &value).unwrap();
        let packed = SharedPackedValue::copy_from(
            encoded.as_bytes(),
            &TypeSignature::type_of(&value).unwrap(),
            &epoch,
        )
        .unwrap();
        let packed_address = packed.as_view().as_sequence_bytes().unwrap().as_ptr();
        let list = SharedPackedValue::list(vec![owned, packed], &epoch).unwrap();
        let tuple = SharedPackedValue::tuple(
            vec![("items".to_string().try_into().unwrap(), list)],
            &epoch,
        )
        .unwrap()
        .into_response(true)
        .unwrap();
        SharedPackedValue::reset_materialization_count();
        let (_, body) = tuple.response_child().unwrap();
        let list = body.tuple_field("items").unwrap().unwrap();
        assert_eq!(
            list.list_child(0)
                .unwrap()
                .unwrap()
                .as_view()
                .as_sequence_bytes()
                .unwrap()
                .as_ptr(),
            address
        );
        assert_eq!(
            list.list_child(1)
                .unwrap()
                .unwrap()
                .as_view()
                .as_sequence_bytes()
                .unwrap()
                .as_ptr(),
            packed_address
        );
        let serialized = tuple.serialize_to_vec().unwrap();
        assert_eq!(SharedPackedValue::materialization_count(), 0);
        let materialized = tuple.to_owned_value().unwrap();
        assert_eq!(serialized, materialized.serialize_to_vec().unwrap());
        assert_eq!(tuple.logical_size().unwrap(), materialized.size().unwrap());
        assert_eq!(tuple.consensus_byte_len(), serialized.len() as u32);
    }

    /// Persistent append stays balanced and old versions remain unchanged.
    #[test]
    fn repeated_append_has_bounded_height_and_persistent_contents() {
        let epoch = StacksEpochId::latest();
        let mut tree = SharedList::items(vec![]);
        let mut snapshots = vec![];
        for i in 0..4096 {
            if i % 511 == 0 {
                snapshots.push(tree.clone());
            }
            let item = SharedPackedValue::from_value(Value::UInt(i), &epoch).unwrap();
            tree = SharedList::concat(tree, SharedList::items(vec![item]));
        }
        assert!(tree.0.height < 20);
        for (i, tree) in snapshots.iter().enumerate() {
            assert_eq!(tree.len(), i * 511);
        }
        for i in 0..tree.len() {
            assert_eq!(tree.view(i).unwrap().unwrap().as_uint(), Some(i as u128));
        }
        assert!(tree.view(4096).unwrap().is_none());
    }

    /// Streaming visits independent byte regions without allocating their coalesced representation.
    #[test]
    fn segmented_bytes_stream_and_single_region_slices_keep_the_original_pointer() {
        let epoch = StacksEpochId::latest();
        let original = vec![7; 8192];
        let address = original.as_ptr();
        let left =
            SharedPackedValue::from_value(Value::buff_from(original).unwrap(), &epoch).unwrap();
        let right = SharedPackedValue::from_value(Value::buff_from(vec![9; 4096]).unwrap(), &epoch)
            .unwrap();
        let value = left.concat_sequence(right, &epoch).unwrap();
        let expected = Value::buff_from([vec![7; 8192], vec![9; 4096]].concat()).unwrap();
        assert_eq!(
            value.serialize_to_vec().unwrap(),
            expected.serialize_to_vec().unwrap()
        );
        let Some(CompositeValue::Bytes { contiguous, .. }) = value.composite.as_deref() else {
            panic!("byte tree")
        };
        assert!(contiguous.get().is_none());
        let slice = value.sliced_sequence(0, 4096).unwrap().unwrap();
        assert_eq!(
            slice.as_view().as_sequence_bytes().unwrap().as_ptr(),
            address
        );
        let replaced = value
            .replace_bytes(
                8191,
                SharedPackedValue::from_value(Value::buff_from(vec![3]).unwrap(), &epoch).unwrap(),
            )
            .unwrap();
        let bytes = replaced.as_view().as_sequence_bytes().unwrap();
        assert_eq!(bytes[8191], 3);
        assert_eq!(bytes[8192], 9);
        assert_eq!(bytes.len(), 12288);
    }

    /// Filters and slices of mixed owners retain the correct child rather than the outer owner.
    #[test]
    fn mixed_selection_keeps_independent_owners() {
        let epoch = StacksEpochId::latest();
        let children = (0..4)
            .map(|i| {
                SharedPackedValue::from_value(Value::buff_from(vec![i; 64]).unwrap(), &epoch)
                    .unwrap()
            })
            .collect::<Vec<_>>();
        let address = children[2].as_view().as_sequence_bytes().unwrap().as_ptr();
        let list = SharedPackedValue::list(children, &epoch)
            .unwrap()
            .filtered_list(vec![0, 2, 3])
            .unwrap()
            .sliced_sequence(1, 2)
            .unwrap()
            .unwrap();
        assert_eq!(
            list.list_child(0)
                .unwrap()
                .unwrap()
                .as_view()
                .as_sequence_bytes()
                .unwrap()
                .as_ptr(),
            address
        );
        assert_eq!(
            list.serialize_to_vec().unwrap(),
            list.to_owned_value().unwrap().serialize_to_vec().unwrap()
        );
    }
}

impl SharedList {
    /// Retain a logical range while pruning branches outside it.
    fn slice(&self, start: usize, end: usize) -> Self {
        if start == 0 && end == self.len() {
            return self.clone();
        }
        if start == end {
            return Self::items(vec![]);
        }
        match &self.0.body {
            ListBody::Items(items) => Self::items(items[start..end].to_vec()),
            ListBody::Source(source) => {
                let value = if source.as_view().as_sequence_bytes().is_some() {
                    source
                        .sliced_sequence(start, end)
                        .expect("source range")
                        .expect("byte slice")
                } else {
                    source.clone().select_list(
                        super::ListSelection::Range(start..end),
                        source.expected.clone(),
                    )
                };
                let byte_len = value
                    .as_view()
                    .as_sequence_bytes()
                    .map_or(0, |bytes| bytes.len());
                Self(Arc::new(ListNode {
                    len: end - start,
                    byte_len,
                    height: 1,
                    body: ListBody::Source(value),
                }))
            }
            ListBody::Branch(left, right) => {
                if end <= left.len() {
                    left.slice(start, end)
                } else if start >= left.len() {
                    right.slice(start - left.len(), end - left.len())
                } else {
                    Self::concat(
                        left.slice(start, left.len()),
                        right.slice(0, end - left.len()),
                    )
                }
            }
        }
    }
}

impl SharedPackedValue {
    /// Append one admitted child while retaining the unchanged list prefix.
    pub fn append_list(self, child: Self, schema: ListTypeData) -> Result<Self, PackedValueError> {
        let epoch = self.epoch;
        let tree = SharedList::concat(SharedList::source(self)?, SharedList::items(vec![child]));
        Ok(Self::from_composite(
            CompositeValue::List(tree),
            TypeSignature::SequenceType(SequenceSubtype::ListType(schema)),
            epoch,
        ))
    }

    /// Replace one list item, preserving the source's historical logical bounds.
    pub fn replace_list(self, index: usize, child: Self) -> Result<Self, PackedValueError> {
        let epoch = self.epoch;
        let expected = self.expected.clone();
        let tree = SharedList::source(self)?;
        if index >= tree.len() {
            return Err(ClarityTypeError::ValueOutOfBounds.into());
        }
        let result = SharedList::concat(
            SharedList::concat(tree.slice(0, index), SharedList::items(vec![child])),
            tree.slice(index + 1, tree.len()),
        );
        Ok(Self::from_composite(
            CompositeValue::List(result),
            expected,
            epoch,
        ))
    }

    /// Merge tuple fields with the same epoch-specific bounds behavior as the owned constructor.
    pub fn merge_tuple(
        self,
        update: Self,
        epoch: &StacksEpochId,
    ) -> Result<Self, PackedValueError> {
        let TypeSignature::TupleType(mut schema) = self.logical_type()? else {
            return Err(PackedValueError::BorrowedView("expected tuple"));
        };
        let TypeSignature::TupleType(mut update_schema) = update.logical_type()? else {
            return Err(PackedValueError::BorrowedView("expected tuple"));
        };
        schema.shallow_merge(&mut update_schema)?;
        let fields = schema
            .get_type_map()
            .keys()
            .map(|name| {
                let value = update
                    .tuple_field(name)?
                    .or(self.tuple_field(name)?)
                    .ok_or(PackedValueError::BorrowedView("missing tuple field"))?;
                Ok((name.clone(), value))
            })
            .collect::<Result<Vec<_>, PackedValueError>>()?;
        Ok(Self::from_composite(
            CompositeValue::Tuple(fields),
            TypeSignature::TupleType(schema),
            *epoch,
        ))
    }
}

impl SharedPackedValue {
    /// Apply Clarity 2 argument metadata casts while retaining compound payload owners.
    /// `None` reports a tuple field absent from the parameter schema.
    pub fn implicit_cast(
        self,
        expected: &TypeSignature,
        epoch: &StacksEpochId,
    ) -> Result<Option<Self>, PackedValueError> {
        let result = match (expected, self.kind()?) {
            (TypeSignature::OptionalType(inner), PackedValueKind::Optional) => {
                match self.optional_child()? {
                    Some(child) => {
                        let Some(child) = child.implicit_cast(inner, epoch)? else {
                            return Ok(None);
                        };
                        child.into_optional()?
                    }
                    None => self,
                }
            }
            (TypeSignature::ResponseType(types), PackedValueKind::Response) => {
                let (committed, child) = self.response_child()?;
                let Some(child) =
                    child.implicit_cast(if committed { &types.0 } else { &types.1 }, epoch)?
                else {
                    return Ok(None);
                };
                child.into_response(committed)?
            }
            (
                TypeSignature::SequenceType(SequenceSubtype::ListType(target)),
                PackedValueKind::List,
            ) => {
                let TypeSignature::SequenceType(SequenceSubtype::ListType(source)) =
                    self.logical_type()?
                else {
                    unreachable!("list kind")
                };
                let schema = ListTypeData::new_list(
                    target.get_list_item_type().clone(),
                    source.get_max_len(),
                )?;
                let len = self.as_view().as_list()?.len();
                let mut items = Vec::with_capacity(len);
                for i in 0..len {
                    let Some(child) = self
                        .list_child(i)?
                        .expect("list index")
                        .implicit_cast(target.get_list_item_type(), epoch)?
                    else {
                        return Ok(None);
                    };
                    items.push(child);
                }
                Self::from_composite(
                    CompositeValue::List(SharedList::items(items)),
                    TypeSignature::SequenceType(SequenceSubtype::ListType(schema)),
                    *epoch,
                )
            }
            (TypeSignature::TupleType(target), PackedValueKind::Tuple) => {
                let tuple = self.as_view().as_tuple()?;
                let mut fields = Vec::with_capacity(tuple.len());
                for i in 0..tuple.len() {
                    let (name, _) = tuple.get_index(i)?.expect("tuple index");
                    let Some(ty) = target.get_type_map().get(name) else {
                        return Ok(None);
                    };
                    let Some(child) = self
                        .tuple_field(name)?
                        .expect("tuple field")
                        .implicit_cast(ty, epoch)?
                    else {
                        return Ok(None);
                    };
                    fields.push((name.clone(), child));
                }
                // The owned cast preserves the declared tuple schema even in historical cases
                // with fewer active fields. Retain that distinction until sanitization/admission.
                Self::from_composite(CompositeValue::Tuple(fields), expected.clone(), *epoch)
            }
            (
                TypeSignature::CallableType(CallableSubtype::Trait(identifier)),
                PackedValueKind::Principal | PackedValueKind::Callable,
            ) => {
                let value = self.as_view().to_owned_value()?;
                let contract = match value {
                    Value::Principal(PrincipalData::Contract(contract)) => Some(contract),
                    Value::CallableContract(callable) => Some(callable.contract_identifier),
                    _ => None,
                };
                match contract {
                    Some(contract_identifier) => Self::from_value(
                        Value::CallableContract(CallableData {
                            contract_identifier,
                            trait_identifier: Some(Box::new(identifier.clone())),
                        }),
                        epoch,
                    )?,
                    None => self,
                }
            }
            _ => self,
        };
        Ok(Some(result))
    }
}

impl SharedList {
    /// Physical payload size of a byte-sequence tree.
    pub fn byte_len(&self) -> usize {
        self.0.byte_len
    }

    /// Stream byte segments in logical order without coalescing them first.
    pub fn write_bytes<W: Write>(&self, writer: &mut W) -> io::Result<()> {
        match &self.0.body {
            ListBody::Source(source) => writer.write_all(
                source
                    .as_view()
                    .as_sequence_bytes()
                    .expect("byte tree leaf"),
            ),
            ListBody::Branch(left, right) => {
                left.write_bytes(writer)?;
                right.write_bytes(writer)
            }
            ListBody::Items(items) => {
                assert!(items.is_empty(), "byte tree has no list items");
                Ok(())
            }
        }
    }
}

impl CompositeValue {
    /// Borrow a contiguous payload, allocating only when a segmented consumer requires it.
    pub fn sequence_bytes(&self) -> Option<&[u8]> {
        let Self::Bytes { tree, contiguous } = self else {
            return None;
        };
        match &tree.0.body {
            ListBody::Source(source) => return source.as_view().as_sequence_bytes(),
            ListBody::Items(items) if items.is_empty() => return Some(&[]),
            _ => {}
        }
        Some(contiguous.get_or_init(|| {
            stacks_profiler::diagnostics::count("byte_coalesces", 1);
            stacks_profiler::diagnostics::count("byte_coalesced_bytes", tree.byte_len() as u64);
            let mut bytes = Vec::with_capacity(tree.byte_len());
            tree.write_bytes(&mut bytes).expect("write to vec");
            bytes
        }))
    }
}

impl SharedPackedValue {
    /// Return a virtual byte sequence's logical length without coalescing payloads.
    pub fn segmented_sequence_len(&self) -> Option<usize> {
        match self.composite.as_deref() {
            Some(CompositeValue::Bytes { tree, .. }) => Some(tree.len()),
            _ => None,
        }
    }

    /// Slice a segmented sequence while retaining its original payload owners.
    pub fn slice_segmented(
        &self,
        start: usize,
        end: usize,
    ) -> Result<Option<Self>, PackedValueError> {
        let Some(CompositeValue::Bytes { tree, .. }) = self.composite.as_deref() else {
            return Ok(None);
        };
        if start > end || end > tree.len() {
            return Err(ClarityTypeError::ValueOutOfBounds.into());
        }
        let expected = byte_sequence_type(self.kind()?, end - start)?;
        Ok(Some(Self::from_composite(
            CompositeValue::Bytes {
                tree: tree.slice(start, end),
                contiguous: OnceLock::new(),
            },
            expected,
            self.epoch,
        )))
    }

    /// Concatenate matching sequences while retaining their source allocations.
    pub fn concat_sequence(
        self,
        other: Self,
        epoch: &StacksEpochId,
    ) -> Result<Self, PackedValueError> {
        let kind = self.kind()?;
        if kind != other.kind()? {
            return Err(ClarityTypeError::TypeMismatch(
                Box::new(self.logical_type()?),
                Box::new(other.logical_type()?),
            )
            .into());
        }
        if kind == PackedValueKind::List {
            let TypeSignature::SequenceType(SequenceSubtype::ListType(left)) =
                self.logical_type()?
            else {
                unreachable!("list kind")
            };
            let TypeSignature::SequenceType(SequenceSubtype::ListType(right)) =
                other.logical_type()?
            else {
                unreachable!("list kind")
            };
            let item_type = TypeSignature::factor_out_no_type(
                epoch,
                left.get_list_item_type(),
                right.get_list_item_type(),
            )?;
            let mut children = Vec::with_capacity(other.as_view().as_list()?.len());
            for i in 0..other.as_view().as_list()?.len() {
                children.push(
                    other
                        .list_child(i)?
                        .expect("list index")
                        .sanitize(epoch, &item_type)
                        .ok_or(ClarityTypeError::ListTypeMismatch)?
                        .0,
                );
            }
            let schema =
                ListTypeData::new_list(item_type, left.get_max_len() + right.get_max_len())?;
            let tree = SharedList::concat(SharedList::source(self)?, SharedList::items(children));
            return Ok(Self::from_composite(
                CompositeValue::List(tree),
                TypeSignature::SequenceType(SequenceSubtype::ListType(schema)),
                *epoch,
            ));
        }
        let tree = SharedList::concat(SharedList::source(self)?, SharedList::source(other)?);
        let expected = byte_sequence_type(kind, tree.len())?;
        Ok(Self::from_composite(
            CompositeValue::Bytes {
                tree,
                contiguous: OnceLock::new(),
            },
            expected,
            *epoch,
        ))
    }

    /// Replace a single buffer byte or string character without copying the unchanged regions.
    pub fn replace_bytes(self, index: usize, element: Self) -> Result<Self, PackedValueError> {
        let kind = self.kind()?;
        let epoch = self.epoch;
        let source = SharedList::source(self)?;
        let replacement = SharedList::source(element)?;
        if replacement.len() != 1 {
            return Err(ClarityTypeError::SequenceElementArityMismatch {
                expected: 1,
                found: replacement.len(),
            }
            .into());
        }
        let expected = byte_sequence_type(kind, source.len())?;
        let tree = SharedList::concat(
            SharedList::concat(source.slice(0, index), replacement),
            source.slice(index + 1, source.len()),
        );
        Ok(Self::from_composite(
            CompositeValue::Bytes {
                tree,
                contiguous: OnceLock::new(),
            },
            expected,
            epoch,
        ))
    }
}

/// Derive a byte sequence's logical type from its logical element count.
fn byte_sequence_type(
    kind: PackedValueKind,
    len: usize,
) -> Result<TypeSignature, PackedValueError> {
    Ok(TypeSignature::SequenceType(match kind {
        PackedValueKind::Buffer => SequenceSubtype::BufferType(BufferLength::try_from(len)?),
        PackedValueKind::Ascii => {
            SequenceSubtype::StringType(StringSubtype::ASCII(BufferLength::try_from(len)?))
        }
        PackedValueKind::Utf8 => {
            SequenceSubtype::StringType(StringSubtype::UTF8(StringUTF8Length::try_from(len)?))
        }
        _ => return Err(PackedValueError::BorrowedView("expected byte sequence")),
    }))
}
