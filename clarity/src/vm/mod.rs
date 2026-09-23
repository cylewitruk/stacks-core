// Copyright (C) 2013-2020 Blockstack PBC, a public benefit corporation
// Copyright (C) 2020-2026 Stacks Open Internet Foundation
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
pub mod diagnostic;
pub mod errors;

#[macro_use]
pub mod costs;

pub mod types;

pub mod contracts;

pub mod ast;
pub mod contexts;
pub mod database;
pub mod hooks;
pub mod representations;

pub mod callables;
pub mod functions;
pub mod resource_limiter;
pub mod variables;

pub mod analysis;
pub mod docs;
pub mod version;

pub mod events;

#[cfg(feature = "rusqlite")]
pub mod tooling;

#[cfg(any(test, feature = "testing"))]
pub mod tests;

#[cfg(any(test, feature = "testing"))]
pub mod test_util;

pub mod clarity;

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::ops::Deref;

pub use clarity_types::max_call_stack_depth_for_epoch;
use stacks_common::bounded_format;
use stacks_common::types::StacksEpochId;

use self::analysis::ContractAnalysis;
use self::ast::ContractAST;
use self::costs::ExecutionCost;
use self::diagnostic::Diagnostic;
use crate::vm::callables::{BuiltinKind, CallableType, FunctionIdentifier};
pub use crate::vm::contexts::{CallStack, ContractContext, LocalContext, MAX_CONTEXT_DEPTH};
use crate::vm::contexts::{ExecutionState, GlobalContext, InvocationContext};
use crate::vm::costs::cost_functions::ClarityCostFunction;
use crate::vm::costs::{
    CostErrors, CostOverflowingMath, CostTracker, LimitedCostTracker, MemoryConsumer, runtime_cost,
};
// publish the non-generic StacksEpoch form for use throughout module
pub use crate::vm::database::clarity_db::StacksEpoch;
#[cfg(any(test, feature = "testing"))]
use crate::vm::errors::ClarityEvalError;
use crate::vm::errors::{RuntimeCheckErrorKind, RuntimeError, VmExecutionError, VmInternalError};
use crate::vm::events::StacksTransactionEvent;
use crate::vm::functions::define::DefineResult;
pub use crate::vm::functions::stx_transfer_consolidated;
use crate::vm::hooks::{CallArguments, CallTraceFrame, EvalHookNotifier as _};
pub use crate::vm::representations::{
    ClarityName, ContractName, SymbolicExpression, SymbolicExpressionType,
};
#[cfg(any(test, feature = "testing"))]
use crate::vm::resource_limiter::ResourceBudget;
use crate::vm::resource_limiter::ResourceLimitExceeded;
pub use crate::vm::types::Value;
use crate::vm::types::codec::packed::{PackedValueError, PackedValueKind, SharedPackedValue};
use crate::vm::types::{CharType, PrincipalData, SequenceData, TypeSignature};
pub use crate::vm::version::ClarityVersion;

/// A wrapper for variable value references that prevents accidental cloning.
/// Only explicit clone_with_cost is allowed. Do not implement Clone or Copy for this type.
#[derive(Debug, PartialEq)]
pub enum ValueRef<'a> {
    Borrowed(&'a Value),
    Owned(Value),
    /// A shared packed or composite value that materializes only for owned consumers.
    Packed(PackedValueCow<'a>),
}

/// A normalized read-only view over legacy and packed value representations.
enum ValueRepresentation<'a> {
    /// An already-materialized legacy value tree, regardless of its ownership.
    Legacy(&'a Value),
    /// A shared packed value record.
    Packed(&'a SharedPackedValue),
}

/// A stable packed value plus the historical local-variable clone-cost provenance.
#[derive(Debug, PartialEq)]
pub struct PackedValueCow<'a> {
    /// Borrowed lexical binding or owned shared-record handle.
    value: Cow<'a, SharedPackedValue>,
    /// Whether consuming this reference historically cloned a local variable.
    charge_clone_cost: bool,
}

impl<'a> PackedValueCow<'a> {
    /// Wrap a backing-store result, which is already an evaluated owned result.
    fn stored(value: SharedPackedValue) -> Self {
        Self {
            value: Cow::Owned(value),
            charge_clone_cost: false,
        }
    }

    /// Borrow a lexical binding whose historical owned path charged for cloning.
    fn binding(value: &'a SharedPackedValue) -> Self {
        Self {
            value: Cow::Borrowed(value),
            charge_clone_cost: true,
        }
    }

    /// Preserve the parent's clone-cost provenance on a shared child projection.
    fn projected(&self, value: SharedPackedValue) -> Self {
        Self {
            value: Cow::Owned(value),
            charge_clone_cost: self.charge_clone_cost,
        }
    }

    /// Consume the wrapper into a lifetime-independent shared-record handle.
    fn into_shared(self) -> SharedPackedValue {
        self.value.into_owned()
    }

    /// Return the legacy local-binding clone-cost input, when applicable.
    fn clone_cost_size(&self) -> Result<Option<u64>, VmExecutionError> {
        self.charge_clone_cost
            .then(|| self.logical_size().map(u64::from))
            .transpose()
            .map_err(packed_vm_error)
    }
}

impl Deref for PackedValueCow<'_> {
    type Target = SharedPackedValue;

    fn deref(&self) -> &Self::Target {
        self.value.as_ref()
    }
}

/// An owned VM binding that either owns a legacy value tree or shares immutable packed storage.
#[derive(Debug, PartialEq)]
pub enum ValueCow {
    /// Existing recursive runtime representation.
    Owned(Value),
    /// Stable shared Binary V1 representation.
    Packed(SharedPackedValue),
}

impl ValueCow {
    /// Borrow this binding through the evaluator's value-reference abstraction.
    pub fn as_value_ref(&self) -> ValueRef<'_> {
        match self {
            ValueCow::Owned(value) => ValueRef::Borrowed(value),
            ValueCow::Packed(value) => ValueRef::Packed(PackedValueCow::binding(value)),
        }
    }

    /// Borrow the compatibility representation, materializing packed storage lazily.
    pub fn as_value(&self) -> &Value {
        match self {
            ValueCow::Owned(value) => value,
            ValueCow::Packed(value) => value.materialized_infallible(),
        }
    }
}

impl From<Value> for ValueCow {
    fn from(value: Value) -> Self {
        Self::Owned(value)
    }
}

impl AsRef<Value> for ValueRef<'_> {
    #[cfg_attr(any(test, feature = "testing"), track_caller)]
    fn as_ref(&self) -> &Value {
        match self.representation() {
            ValueRepresentation::Legacy(value) => value,
            ValueRepresentation::Packed(value) => value.materialized_infallible(),
        }
    }
}

impl<'a> ValueRef<'a> {
    /// Normalize ownership-insensitive access to this value's representation.
    fn representation(&self) -> ValueRepresentation<'_> {
        match self {
            Self::Borrowed(value) => ValueRepresentation::Legacy(value),
            Self::Owned(value) => ValueRepresentation::Legacy(value),
            Self::Packed(value) => ValueRepresentation::Packed(value),
        }
    }

    /// Retain an evaluated value in the common shared representation.
    pub fn into_shared(self, epoch: &StacksEpochId) -> Result<SharedPackedValue, VmExecutionError> {
        match self {
            Self::Packed(value) => Ok(value.into_shared()),
            value => SharedPackedValue::from_value(value.into_owned()?, epoch)
                .map_err(composite_vm_error),
        }
    }

    /// Wrap an admitted shared value with native-result cost provenance.
    pub fn from_shared(value: SharedPackedValue) -> ValueRef<'static> {
        ValueRef::Packed(PackedValueCow::stored(value))
    }

    /// Whether this reference carries a non-scalar payload worth retaining in an aggregate.
    pub fn retains_payload(&self) -> bool {
        matches!(self, ValueRef::Packed(value) if !matches!(value.expected(), TypeSignature::IntType | TypeSignature::UIntType | TypeSignature::BoolType))
    }

    /// Construct a list, retaining shared children when any input is borrowed storage.
    pub fn list_from(
        values: Vec<ValueRef<'_>>,
        epoch: &StacksEpochId,
    ) -> Result<ValueRef<'static>, VmExecutionError> {
        if values.iter().any(ValueRef::retains_payload) {
            let children = values
                .into_iter()
                .map(|value| value.into_shared(epoch))
                .collect::<Result<Vec<_>, _>>()?;
            SharedPackedValue::list(children, epoch)
                .map(Self::from_shared)
                .map_err(composite_vm_error)
        } else {
            let children = values
                .into_iter()
                .map(ValueRef::into_owned)
                .collect::<Result<Vec<_>, _>>()?;
            Ok(ValueRef::Owned(Value::cons_list(children, epoch)?))
        }
    }

    /// Serialize directly from the active representation.
    pub fn serialize_to_vec(
        &self,
    ) -> Result<Vec<u8>, crate::vm::types::serialization::SerializationError> {
        match self.representation() {
            ValueRepresentation::Legacy(value) => value.serialize_to_vec(),
            ValueRepresentation::Packed(value) => value.serialize_to_vec(),
        }
    }

    /// Apply historical sanitization without materializing retained compound payloads.
    pub fn sanitize(
        self,
        epoch: &StacksEpochId,
        expected: &TypeSignature,
    ) -> Result<Option<(ValueRef<'static>, bool)>, VmExecutionError> {
        match self {
            Self::Packed(value) => Ok(value
                .into_shared()
                .sanitize(epoch, expected)
                .map(|(value, changed)| (Self::from_shared(value), changed))),
            value => Ok(Value::sanitize_value(epoch, expected, value.into_owned()?)
                .map(|(value, changed)| (ValueRef::Owned(value), changed))),
        }
    }

    /// Serialize a map key using the same canonical lowercase hex representation.
    pub fn serialize_to_hex(
        &self,
    ) -> Result<String, crate::vm::types::serialization::SerializationError> {
        self.serialize_to_vec()
            .map(|bytes| stacks_common::util::hash::to_hex(&bytes))
    }

    /// Construct an owned or shared response without changing the child's cost provenance.
    pub fn into_response(self, committed: bool) -> Result<ValueRef<'static>, VmExecutionError> {
        match self {
            Self::Packed(value) => value
                .into_shared()
                .into_response(committed)
                .map(Self::from_shared)
                .map_err(composite_vm_error),
            value => {
                let value = value.into_owned()?;
                Ok(ValueRef::Owned(if committed {
                    Value::okay(value)?
                } else {
                    Value::error(value)?
                }))
            }
        }
    }

    /// Charge the historical argument-clone cost while retaining borrowed storage.
    pub fn charge_clone_cost<T: CostTracker>(
        &self,
        tracker: &mut T,
    ) -> Result<(), VmExecutionError> {
        let size = match self {
            ValueRef::Borrowed(value) => Some(u64::from(value.size()?)),
            ValueRef::Packed(value) => value.clone_cost_size()?,
            ValueRef::Owned(_) => None,
        };
        if let Some(size) = size {
            runtime_cost(ClarityCostFunction::LookupVariableSize, tracker, size)?;
        }
        Ok(())
    }

    /// Materialize or clone this value without applying an additional runtime charge.
    pub fn into_owned(self) -> Result<Value, VmExecutionError> {
        match self {
            ValueRef::Borrowed(value) => Ok(value.clone()),
            ValueRef::Owned(value) => Ok(value),
            ValueRef::Packed(value) => {
                let view = value.as_view();
                if let Some(value) = view.as_uint() {
                    return Ok(Value::UInt(value));
                }
                if let Some(value) = view.as_int() {
                    return Ok(Value::Int(value));
                }
                if let Some(value) = view.as_bool() {
                    return Ok(Value::Bool(value));
                }
                value.to_owned_value().map_err(packed_vm_error)
            }
        }
    }

    /// Retain shared packed storage with the cost provenance of an evaluated native result.
    pub fn into_evaluated(self) -> Result<ValueRef<'static>, VmExecutionError> {
        match self {
            ValueRef::Packed(value) => Ok(ValueRef::Packed(PackedValueCow::stored(
                value.into_shared(),
            ))),
            value => value.into_owned().map(ValueRef::Owned),
        }
    }

    /// Move this value into a lifetime-independent lexical binding.
    pub fn into_cow(self) -> ValueCow {
        match self {
            ValueRef::Borrowed(value) => ValueCow::Owned(value.clone()),
            ValueRef::Owned(value) => ValueCow::Owned(value),
            ValueRef::Packed(value) => ValueCow::Packed(value.into_shared()),
        }
    }

    /// Make this value independent of lexical borrows while retaining shared packed storage.
    pub fn into_static<T: CostTracker>(
        self,
        tracker: &mut T,
    ) -> Result<ValueRef<'static>, VmExecutionError> {
        self.charge_clone_cost(tracker)?;
        self.into_evaluated()
    }

    /// Return whether this packed value was decoded under exactly the target schema.
    pub fn has_packed_schema(&self, expected: &TypeSignature) -> bool {
        match self {
            ValueRef::Packed(value) => value.expected() == expected,
            ValueRef::Borrowed(_) | ValueRef::Owned(_) => false,
        }
    }

    /// Return this value's logical size without materializing a packed value.
    pub fn size(&self) -> Result<u32, VmExecutionError> {
        match self.representation() {
            ValueRepresentation::Legacy(value) => value.size().map_err(VmExecutionError::from),
            ValueRepresentation::Packed(value) => value.logical_size().map_err(packed_vm_error),
        }
    }

    /// Return this value's logical memory charge without materializing a packed value.
    pub fn get_memory_use(&self) -> Result<u64, VmExecutionError> {
        self.size().map(u64::from)
    }

    /// Return the consensus-serialized length used by size-based runtime costs.
    pub fn serialized_byte_len(&self) -> Result<u32, VmExecutionError> {
        match self.representation() {
            ValueRepresentation::Legacy(value) => value
                .serialized_size()
                .map_err(|error| CostErrors::Expect(format!("{error:?}")).into()),
            ValueRepresentation::Packed(value) => Ok(value.consensus_byte_len()),
        }
    }

    /// Return an unsigned integer scalar without materializing packed storage.
    pub fn as_uint(&self) -> Result<Option<u128>, VmExecutionError> {
        match self.representation() {
            ValueRepresentation::Legacy(Value::UInt(value)) => Ok(Some(*value)),
            ValueRepresentation::Legacy(_) => Ok(None),
            ValueRepresentation::Packed(value) => Ok(value.as_view().as_uint()),
        }
    }

    /// Return a signed integer scalar without materializing packed storage.
    pub fn as_int(&self) -> Result<Option<i128>, VmExecutionError> {
        match self.representation() {
            ValueRepresentation::Legacy(Value::Int(value)) => Ok(Some(*value)),
            ValueRepresentation::Legacy(_) => Ok(None),
            ValueRepresentation::Packed(value) => Ok(value.as_view().as_int()),
        }
    }

    /// Return a Boolean scalar without materializing packed storage.
    pub fn as_bool(&self) -> Result<Option<bool>, VmExecutionError> {
        match self.representation() {
            ValueRepresentation::Legacy(Value::Bool(value)) => Ok(Some(*value)),
            ValueRepresentation::Legacy(_) => Ok(None),
            ValueRepresentation::Packed(value) => Ok(value.as_view().as_bool()),
        }
    }

    /// Borrow a buffer payload without materializing packed storage.
    pub fn as_buffer_bytes(&self) -> Result<Option<&[u8]>, VmExecutionError> {
        match self.representation() {
            ValueRepresentation::Legacy(Value::Sequence(
                crate::vm::types::SequenceData::Buffer(value),
            )) => Ok(Some(&value.data)),
            ValueRepresentation::Legacy(_) => Ok(None),
            ValueRepresentation::Packed(value) => {
                let view = value.as_view();
                Ok(
                    (view.kind().map_err(packed_vm_error)? == PackedValueKind::Buffer)
                        .then(|| view.as_sequence_bytes())
                        .flatten(),
                )
            }
        }
    }

    /// Borrow packed/ASCII text and flatten only the legacy UTF-8 representation when needed.
    pub fn as_text(&self) -> Result<Option<Cow<'_, str>>, VmExecutionError> {
        match self.representation() {
            ValueRepresentation::Legacy(Value::Sequence(SequenceData::String(
                CharType::ASCII(value),
            ))) => std::str::from_utf8(&value.data)
                .map(Cow::Borrowed)
                .map(Some)
                .map_err(|_| {
                    VmInternalError::Expect("validated ASCII value contains invalid text".into())
                        .into()
                }),
            ValueRepresentation::Legacy(Value::Sequence(SequenceData::String(CharType::UTF8(
                value,
            )))) => String::from_utf8(value.data.iter().flatten().copied().collect())
                .map(Cow::Owned)
                .map(Some)
                .map_err(|_| {
                    VmInternalError::Expect("validated UTF-8 value contains invalid text".into())
                        .into()
                }),
            ValueRepresentation::Packed(value) => {
                let view = value.as_view();
                match view.kind().map_err(packed_vm_error)? {
                    PackedValueKind::Ascii => {
                        std::str::from_utf8(view.as_sequence_bytes().ok_or_else(|| {
                            packed_vm_error(PackedValueError::BorrowedView("type mismatch"))
                        })?)
                        .map(Cow::Borrowed)
                        .map(Some)
                        .map_err(|_| {
                            VmInternalError::Expect(
                                "validated packed ASCII contains invalid text".into(),
                            )
                            .into()
                        })
                    }
                    PackedValueKind::Utf8 => Ok(view.as_utf8().map(Cow::Borrowed)),
                    _ => Ok(None),
                }
            }
            ValueRepresentation::Legacy(_) => Ok(None),
        }
    }

    /// Borrow packed/ASCII text bytes, joining an owned UTF-8 value only when needed.
    pub fn as_text_bytes(&self) -> Result<Option<Cow<'_, [u8]>>, VmExecutionError> {
        Ok(match self.representation() {
            ValueRepresentation::Legacy(Value::Sequence(SequenceData::String(
                CharType::ASCII(value),
            ))) => Some(Cow::Borrowed(value.data.as_slice())),
            ValueRepresentation::Legacy(Value::Sequence(SequenceData::String(CharType::UTF8(
                value,
            )))) => Some(Cow::Owned(value.data.iter().flatten().copied().collect())),
            ValueRepresentation::Packed(value) => {
                let view = value.as_view();
                match view.kind().map_err(packed_vm_error)? {
                    PackedValueKind::Ascii | PackedValueKind::Utf8 => {
                        view.as_sequence_bytes().map(Cow::Borrowed)
                    }
                    _ => None,
                }
            }
            ValueRepresentation::Legacy(_) => None,
        })
    }

    /// Return a sequence's logical item count without materializing packed storage.
    pub fn sequence_len(&self) -> Result<Option<usize>, VmExecutionError> {
        match self.representation() {
            ValueRepresentation::Legacy(Value::Sequence(value)) => Ok(Some(value.len())),
            ValueRepresentation::Legacy(_) => Ok(None),
            ValueRepresentation::Packed(value) => {
                if let Some(len) = value.segmented_sequence_len() {
                    return Ok(Some(len));
                }
                let view = value.as_view();
                Ok(Some(match view.kind().map_err(packed_vm_error)? {
                    PackedValueKind::Buffer | PackedValueKind::Ascii => view
                        .as_sequence_bytes()
                        .ok_or_else(|| {
                            packed_vm_error(PackedValueError::BorrowedView("type mismatch"))
                        })?
                        .len(),
                    PackedValueKind::Utf8 => value.utf8_len().ok_or_else(|| {
                        packed_vm_error(PackedValueError::BorrowedView("type mismatch"))
                    })?,
                    PackedValueKind::List => view.as_list().map_err(packed_vm_error)?.len(),
                    _ => return Ok(None),
                }))
            }
        }
    }

    /// Project one element, retaining packed compound children without decoding them.
    pub fn sequence_element_ref(self, index: usize) -> Result<Option<Self>, VmExecutionError> {
        match self {
            ValueRef::Borrowed(Value::Sequence(crate::vm::types::SequenceData::Buffer(value))) => {
                Ok(value
                    .data
                    .get(index)
                    .copied()
                    .map(|byte| ValueRef::Owned(Value::buff_from_byte(byte))))
            }
            ValueRef::Borrowed(Value::Sequence(crate::vm::types::SequenceData::List(value))) => {
                Ok(value.data.get(index).map(ValueRef::Borrowed))
            }
            ValueRef::Borrowed(Value::Sequence(crate::vm::types::SequenceData::String(
                crate::vm::types::CharType::ASCII(value),
            ))) => value
                .data
                .get(index)
                .copied()
                .map(|byte| Value::string_ascii_from_bytes(vec![byte]))
                .transpose()
                .map(|value| value.map(ValueRef::Owned))
                .map_err(VmExecutionError::from),
            ValueRef::Borrowed(Value::Sequence(crate::vm::types::SequenceData::String(
                crate::vm::types::CharType::UTF8(value),
            ))) => value
                .data
                .get(index)
                .cloned()
                .map(Value::string_utf8_from_bytes)
                .transpose()
                .map(|value| value.map(ValueRef::Owned))
                .map_err(VmExecutionError::from),
            ValueRef::Owned(Value::Sequence(value)) => value
                .element_at(index)
                .map(|value| value.map(ValueRef::Owned))
                .map_err(VmExecutionError::from),
            ValueRef::Packed(value) => {
                if let Some(len) = value.segmented_sequence_len() {
                    if index >= len {
                        return Ok(None);
                    }
                    return value
                        .sliced_sequence(index, index + 1)
                        .map(|child| child.map(ValueRef::from_shared))
                        .map_err(composite_vm_error);
                }
                let view = value.as_view();
                match view.kind().map_err(packed_vm_error)? {
                    PackedValueKind::Buffer => Ok(view
                        .as_sequence_bytes()
                        .and_then(|bytes| bytes.get(index).copied())
                        .map(|byte| ValueRef::Owned(Value::buff_from_byte(byte)))),
                    PackedValueKind::Ascii => Ok(view
                        .as_sequence_bytes()
                        .and_then(|bytes| bytes.get(index).copied())
                        .map(|byte| {
                            ValueRef::Owned(
                                Value::string_ascii_from_bytes(vec![byte])
                                    .expect("validated ASCII remains valid when projected"),
                            )
                        })),
                    PackedValueKind::Utf8 => view
                        .utf8_element(index)
                        .map(|bytes| Value::string_utf8_from_bytes(bytes.to_vec()))
                        .transpose()
                        .map(|value| value.map(ValueRef::Owned))
                        .map_err(VmExecutionError::from),
                    PackedValueKind::List => {
                        let Some(child) = view
                            .as_list()
                            .map_err(packed_vm_error)?
                            .get(index)
                            .map_err(packed_vm_error)?
                        else {
                            return Ok(None);
                        };
                        let scalar = match child.kind().map_err(packed_vm_error)? {
                            PackedValueKind::UInt => {
                                Some(Value::UInt(child.as_uint().expect("uint lane")))
                            }
                            PackedValueKind::Int => {
                                Some(Value::Int(child.as_int().expect("int lane")))
                            }
                            PackedValueKind::Bool => {
                                Some(Value::Bool(child.as_bool().expect("bool lane")))
                            }
                            _ => None,
                        };
                        if let Some(scalar) = scalar {
                            return Ok(Some(ValueRef::Owned(scalar)));
                        }
                        value
                            .list_child(index)
                            .map_err(packed_vm_error)
                            .map(|child| {
                                child.map(|child| ValueRef::Packed(PackedValueCow::stored(child)))
                            })
                    }
                    _ => Ok(None),
                }
            }
            other => Err(RuntimeCheckErrorKind::Unreachable(bounded_format!(
                "Expected sequence: {}",
                other.type_signature()?
            ))
            .into()),
        }
    }

    /// Materialize only the selected element for compatibility consumers.
    pub fn sequence_element_at(self, index: usize) -> Result<Option<Value>, VmExecutionError> {
        self.sequence_element_ref(index)?
            .map(ValueRef::into_owned)
            .transpose()
    }

    /// Wrap a result in `some`, retaining packed payload ownership and result cost provenance.
    pub fn into_optional(self) -> Result<ValueRef<'static>, VmExecutionError> {
        match self {
            ValueRef::Packed(value) => Ok(ValueRef::Packed(PackedValueCow::stored(
                value
                    .into_shared()
                    .into_optional()
                    .map_err(|error| match error {
                        PackedValueError::ClarityType(error) => VmExecutionError::from(error),
                        error => packed_vm_error(error),
                    })?,
            ))),
            other => Ok(ValueRef::Owned(Value::some(other.into_owned()?)?)),
        }
    }

    /// Return the active optional child while preserving a packed projection.
    pub fn optional_child(self) -> Result<Option<Self>, VmExecutionError> {
        match self {
            ValueRef::Borrowed(Value::Optional(data)) => {
                Ok(data.data.as_deref().map(ValueRef::Borrowed))
            }
            ValueRef::Owned(Value::Optional(data)) => {
                Ok(data.data.map(|value| ValueRef::Owned(*value)))
            }
            ValueRef::Packed(value) => value
                .optional_child()
                .map(|child| child.map(|child| ValueRef::Packed(value.projected(child))))
                .map_err(packed_vm_error),
            other => Err(RuntimeCheckErrorKind::Unreachable(bounded_format!(
                "Expected optional value: {}",
                other.as_ref()
            ))
            .into()),
        }
    }

    /// Return the active response child and whether it is committed.
    pub fn response_child(self) -> Result<(bool, Self), VmExecutionError> {
        match self {
            ValueRef::Borrowed(Value::Response(data)) => {
                Ok((data.committed, ValueRef::Borrowed(data.data.as_ref())))
            }
            ValueRef::Owned(Value::Response(data)) => {
                Ok((data.committed, ValueRef::Owned(*data.data)))
            }
            ValueRef::Packed(value) => value
                .response_child()
                .map(|(committed, child)| (committed, ValueRef::Packed(value.projected(child))))
                .map_err(packed_vm_error),
            other => Err(RuntimeCheckErrorKind::Unreachable(bounded_format!(
                "Expected response value: {}",
                other.as_ref()
            ))
            .into()),
        }
    }

    /// Return one tuple field while preserving a packed projection.
    pub fn tuple_field(self, name: &str) -> Result<Self, VmExecutionError> {
        match self {
            ValueRef::Borrowed(Value::Tuple(tuple)) => tuple
                .get(name)
                .map(ValueRef::Borrowed)
                .map_err(VmExecutionError::from),
            ValueRef::Owned(Value::Tuple(tuple)) => tuple
                .get_owned(name)
                .map(ValueRef::Owned)
                .map_err(VmExecutionError::from),
            ValueRef::Packed(value) => value
                .tuple_field(name)
                .map_err(packed_vm_error)?
                .map(|child| ValueRef::Packed(value.projected(child)))
                .ok_or_else(|| {
                    RuntimeCheckErrorKind::Unreachable(bounded_format!(
                        "No such tuple field: {name}"
                    ))
                    .into()
                }),
            other => Err(RuntimeCheckErrorKind::Unreachable(bounded_format!(
                "Expected tuple value: {}",
                other.as_ref()
            ))
            .into()),
        }
    }

    /// Return the tuple field count without materializing a packed tuple.
    pub fn tuple_len(&self) -> Result<u64, VmExecutionError> {
        match self.representation() {
            ValueRepresentation::Legacy(Value::Tuple(tuple)) => Ok(tuple.len()),
            ValueRepresentation::Packed(value) => value
                .tuple_len()
                .map(|len| len as u64)
                .map_err(packed_vm_error),
            ValueRepresentation::Legacy(other) => Err(RuntimeCheckErrorKind::Unreachable(
                bounded_format!("Expected tuple value: {}", other),
            )
            .into()),
        }
    }

    /// Report whether this value is an optional without materializing packed storage.
    pub fn is_optional(&self) -> Result<bool, VmExecutionError> {
        match self.representation() {
            ValueRepresentation::Legacy(value) => Ok(matches!(value, Value::Optional(_))),
            ValueRepresentation::Packed(value) => value
                .kind()
                .map(|kind| kind == PackedValueKind::Optional)
                .map_err(packed_vm_error),
        }
    }

    /// Report whether this value is a response without materializing packed storage.
    pub fn is_response(&self) -> Result<bool, VmExecutionError> {
        match self.representation() {
            ValueRepresentation::Legacy(value) => Ok(matches!(value, Value::Response(_))),
            ValueRepresentation::Packed(value) => value
                .kind()
                .map(|kind| kind == PackedValueKind::Response)
                .map_err(packed_vm_error),
        }
    }

    /// Report whether this value is a tuple without materializing packed storage.
    pub fn is_tuple(&self) -> Result<bool, VmExecutionError> {
        match self.representation() {
            ValueRepresentation::Legacy(value) => Ok(matches!(value, Value::Tuple(_))),
            ValueRepresentation::Packed(value) => value
                .kind()
                .map(|kind| kind == PackedValueKind::Tuple)
                .map_err(packed_vm_error),
        }
    }

    /// Report whether this value is a callable contract without materializing packed storage.
    pub fn is_callable(&self) -> Result<bool, VmExecutionError> {
        match self.representation() {
            ValueRepresentation::Legacy(value) => Ok(matches!(value, Value::CallableContract(_))),
            ValueRepresentation::Packed(value) => value
                .kind()
                .map(|kind| kind == PackedValueKind::Callable)
                .map_err(packed_vm_error),
        }
    }

    /// Derive this value's logical type without materializing packed storage.
    pub fn type_signature(&self) -> Result<TypeSignature, VmExecutionError> {
        match self.representation() {
            ValueRepresentation::Legacy(value) => {
                TypeSignature::type_of(value).map_err(VmExecutionError::from)
            }
            ValueRepresentation::Packed(value) => value.logical_type().map_err(packed_vm_error),
        }
    }

    /// Compare logical values, using allocation-free packed recursion when both inputs are packed.
    pub fn value_eq(&self, other: &Self) -> Result<bool, VmExecutionError> {
        match (self.representation(), other.representation()) {
            (ValueRepresentation::Packed(left), ValueRepresentation::Packed(right)) => left
                .as_view()
                .value_eq(right.as_view())
                .map_err(packed_vm_error),
            (ValueRepresentation::Packed(left), ValueRepresentation::Legacy(right)) => left
                .as_view()
                .value_eq_owned(right)
                .map_err(packed_vm_error),
            (ValueRepresentation::Legacy(left), ValueRepresentation::Packed(right)) => right
                .as_view()
                .value_eq_owned(left)
                .map_err(packed_vm_error),
            (ValueRepresentation::Legacy(left), ValueRepresentation::Legacy(right)) => {
                Ok(left == right)
            }
        }
    }

    #[cfg_attr(any(test, feature = "testing"), track_caller)]
    pub fn clone_with_cost<T: CostTracker>(
        self,
        tracker: &mut T,
    ) -> Result<Value, VmExecutionError> {
        match self {
            ValueRef::Borrowed(r) => {
                runtime_cost(ClarityCostFunction::LookupVariableSize, tracker, r.size()?)?;
                Ok(r.clone())
            }
            ValueRef::Owned(o) => Ok(o),
            ValueRef::Packed(value) => {
                if let Some(size) = value.clone_cost_size()? {
                    runtime_cost(ClarityCostFunction::LookupVariableSize, tracker, size)?;
                }
                ValueRef::Packed(value).into_owned()
            }
        }
    }

    #[cfg(test)]
    fn requires_clone_cost(&self) -> bool {
        matches!(
            self,
            ValueRef::Borrowed(_)
                | ValueRef::Packed(PackedValueCow {
                    charge_clone_cost: true,
                    ..
                })
        )
    }
}

/// Preserve ordinary Clarity constructor errors from shared runtime composites.
fn composite_vm_error(error: PackedValueError) -> VmExecutionError {
    match error {
        PackedValueError::ClarityType(error) => error.into(),
        other => packed_vm_error(other),
    }
}

/// Convert a validated packed-codec failure into the VM's storage-error channel.
fn packed_vm_error(error: PackedValueError) -> VmExecutionError {
    VmInternalError::DBError(error.to_string()).into()
}

#[derive(Debug, Clone)]
pub struct ParsedContract {
    pub contract_identifier: String,
    pub code: String,
    pub function_args: BTreeMap<String, Vec<String>>,
    pub ast: ContractAST,
    pub analysis: ContractAnalysis,
}

#[derive(Debug, Clone)]
pub struct ContractEvaluationResult {
    pub result: Option<Value>,
    pub contract: ParsedContract,
}

#[derive(Debug, Clone)]
pub struct SnippetEvaluationResult {
    pub result: Value,
}

#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)]
pub enum EvaluationResult {
    Contract(ContractEvaluationResult),
    Snippet(SnippetEvaluationResult),
}

#[derive(Debug, Clone)]
pub struct ExecutionResult {
    pub result: EvaluationResult,
    pub events: Vec<StacksTransactionEvent>,
    pub cost: Option<CostSynthesis>,
    pub diagnostics: Vec<Diagnostic>,
}

#[derive(Clone, Debug, Serialize)]
pub struct CostSynthesis {
    pub total: ExecutionCost,
    pub limit: ExecutionCost,
    pub memory: u64,
    pub memory_limit: u64,
}

impl CostSynthesis {
    pub fn from_cost_tracker(cost_tracker: &LimitedCostTracker) -> CostSynthesis {
        CostSynthesis {
            total: cost_tracker.get_total(),
            limit: cost_tracker.get_limit(),
            memory: cost_tracker.get_memory(),
            memory_limit: cost_tracker.get_memory_limit(),
        }
    }
}

fn lookup_variable<'a>(
    name: &str,
    exec_state: &mut ExecutionState,
    invoke_ctx: &'a InvocationContext,
    context: &'a LocalContext,
) -> Result<ValueRef<'a>, VmExecutionError> {
    if name.starts_with(char::is_numeric) || name.starts_with('\'') {
        return Err(VmInternalError::BadSymbolicRepresentation(format!(
            "Unexpected variable name: {name}"
        ))
        .into());
    }
    if let Some(value) = variables::lookup_reserved_variable(name, exec_state, invoke_ctx)? {
        return Ok(ValueRef::Owned(value));
    };
    runtime_cost(
        ClarityCostFunction::LookupVariableDepth,
        exec_state,
        context.depth(),
    )?;
    if let Some(value) = context.lookup_variable(name) {
        let value = value.as_value_ref();
        if exec_state.epoch().uses_pre_sanitized_variables() {
            // If the epoch supports value refs, we can return a borrowed reference to the variable without cloning.
            return Ok(value);
        } else {
            // Preserve the legacy clone charge while retaining immutable packed storage.
            return value.into_static(exec_state);
        }
    }
    if let Some(value) = invoke_ctx.contract_context.lookup_variable(name) {
        let value = ValueRef::Borrowed(value);
        if exec_state.epoch().uses_pre_sanitized_variables() {
            // Variables were sanitized at load time by canonicalize_types.
            // Borrow directly.
            return Ok(value);
        }
        // Variables were not sanitized at load time, so we need to sanitize them
        // now before returning and pay for the clone.
        let value = value.clone_with_cost(exec_state)?;
        let (value, _) =
            Value::sanitize_value(exec_state.epoch(), &TypeSignature::type_of(&value)?, value)
                .ok_or_else(|| RuntimeCheckErrorKind::CouldNotDetermineType)?;
        return Ok(ValueRef::Owned(value));
    }
    if let Some(callable_data) = context.lookup_callable_contract(name) {
        let value = if invoke_ctx.contract_context.get_clarity_version() < &ClarityVersion::Clarity2
        {
            callable_data.contract_identifier.clone().into()
        } else {
            Value::CallableContract(callable_data.clone())
        };
        return Ok(ValueRef::Owned(value));
    }
    Err(RuntimeCheckErrorKind::Unreachable(bounded_format!("Undefined variable: {name}")).into())
}

pub fn lookup_function(
    name: &str,
    exec_state: &mut ExecutionState,
    invoke_ctx: &InvocationContext,
) -> Result<CallableType, VmExecutionError> {
    runtime_cost(ClarityCostFunction::LookupFunction, exec_state, 0)?;

    if let Some(result) = functions::lookup_reserved_functions(
        name,
        invoke_ctx.contract_context.get_clarity_version(),
    ) {
        Ok(result)
    } else {
        let user_function = invoke_ctx
            .contract_context
            .lookup_function(name)
            .ok_or(RuntimeCheckErrorKind::UndefinedFunction(name.to_string()))?;
        Ok(CallableType::UserFunction(user_function))
    }
}

fn add_stack_trace<T>(result: &mut Result<T, VmExecutionError>, exec_state: &mut ExecutionState) {
    if let Err(VmExecutionError::Runtime(_, stack_trace)) = result
        && stack_trace.is_none()
    {
        stack_trace.replace(exec_state.call_stack.make_stack_trace());
    }
}

/// Validates recursion and stack-depth invariants common to both [`apply`] and
/// [`apply_evaluated`], returning the function's identifier and whether recursion is tracked.
#[inline]
fn check_call_preconditions(
    function: &CallableType,
    exec_state: &ExecutionState,
) -> Result<(FunctionIdentifier, bool), VmExecutionError> {
    // Aaron: in non-debug executions, we shouldn't track a full call-stack.
    //        only enough to do recursion detection.
    let identifier = function.get_identifier();
    let track_recursion = matches!(function, CallableType::UserFunction(_));
    if track_recursion && exec_state.call_stack.contains(&identifier) {
        return Err(RuntimeCheckErrorKind::CircularReference(vec![identifier.to_string()]).into());
    }
    if exec_state.call_stack.depth() >= max_call_stack_depth_for_epoch(*exec_state.epoch()) {
        return Err(RuntimeError::MaxStackDepthReached.into());
    }
    Ok((identifier, track_recursion))
}

/// Dispatches a pre-evaluated argument list to a non-special [`CallableType`], handling
/// call-stack bookkeeping, cost charging, and memory cleanup.
///
/// Both [`apply`] and [`apply_evaluated`] converge here after preparing their arguments.
/// `used_memory` is the total already charged via [`ExecutionState::add_memory`] for the
/// argument values; it is released before returning.
fn dispatch_args(
    function: &CallableType,
    identifier: FunctionIdentifier,
    track_recursion: bool,
    args: Vec<Value>,
    used_memory: u64,
    exec_state: &mut ExecutionState,
    invoke_ctx: &InvocationContext,
) -> Result<Value, VmExecutionError> {
    exec_state.call_stack.insert(&identifier, track_recursion);

    // Scope `?` to callable execution so the common cleanup below always runs.
    let mut resp = (|| -> Result<Value, VmExecutionError> {
        match function {
            // Built-ins (Native)
            CallableType::Builtin {
                kind: BuiltinKind::Native(_, function, cost_function),
                ..
            } => {
                runtime_cost(cost_function.clone(), exec_state, args.len())
                    .map_err(VmExecutionError::from)?;
                function.apply(args, exec_state, invoke_ctx)
            }

            // Built-ins (Native 2.05+)
            CallableType::Builtin {
                kind: BuiltinKind::Native205(_, function, cost_function, cost_input_handle),
                ..
            } => {
                let cost_input = if exec_state.epoch() >= &StacksEpochId::Epoch2_05 {
                    cost_input_handle(args.as_slice())?
                } else {
                    args.len() as u64
                };

                runtime_cost(cost_function.clone(), exec_state, cost_input)
                    .map_err(VmExecutionError::from)?;
                function.apply(args, exec_state, invoke_ctx)
            }

            // User-defined functions (Clarity)
            CallableType::UserFunction(function) => function.apply(&args, exec_state, invoke_ctx),

            // Special functions evaluate their own arguments and are dispatched directly in
            // `apply`/`apply_evaluated`, so they never reach `dispatch_args`.
            CallableType::Builtin {
                kind: BuiltinKind::Special(..) | BuiltinKind::StoredSpecial(..),
                ..
            } => Err(VmInternalError::Expect("Should be unreachable.".into()).into()),

            // `apply_evaluated` owns these arguments already. Preserve its owned return contract
            // while using the same projection-aware implementation as ordinary evaluation.
            CallableType::Builtin {
                kind: BuiltinKind::BorrowingNative(_, function, cost_function),
                ..
            } => {
                runtime_cost(cost_function.clone(), exec_state, args.len())
                    .map_err(VmExecutionError::from)?;
                function
                    .apply(
                        args.into_iter().map(ValueRef::Owned).collect(),
                        exec_state,
                        invoke_ctx,
                    )?
                    .into_owned()
            }

            CallableType::Builtin {
                kind: BuiltinKind::BorrowingNative205(_, function, cost_function, cost_input_handle),
                ..
            } => {
                let cost_input = if exec_state.epoch() >= &StacksEpochId::Epoch2_05 {
                    let refs: Vec<_> = args.iter().map(ValueRef::Borrowed).collect();
                    cost_input_handle(&refs)?
                } else {
                    args.len() as u64
                };
                runtime_cost(cost_function.clone(), exec_state, cost_input)
                    .map_err(VmExecutionError::from)?;
                function
                    .apply(
                        args.into_iter().map(ValueRef::Owned).collect(),
                        exec_state,
                        invoke_ctx,
                    )?
                    .into_owned()
            }
        }
    })();

    add_stack_trace(&mut resp, exec_state);
    exec_state.drop_memory(used_memory)?;
    exec_state.call_stack.remove(&identifier, track_recursion)?;

    resp
}

/// Dispatch borrowed arguments with the same costs and cleanup as ordinary calls.
fn dispatch_ref_args<'a>(
    function: &CallableType,
    identifier: FunctionIdentifier,
    track_recursion: bool,
    args: Vec<ValueRef<'a>>,
    used_memory: u64,
    exec_state: &mut ExecutionState,
    invoke_ctx: &InvocationContext,
) -> Result<ValueRef<'a>, VmExecutionError> {
    exec_state.call_stack.insert(&identifier, track_recursion);
    let mut resp = match function {
        CallableType::Builtin {
            kind: BuiltinKind::BorrowingNative(_, function, cost_function),
            ..
        } => runtime_cost(cost_function.clone(), exec_state, args.len())
            .map_err(VmExecutionError::from)
            .and_then(|()| function.apply(args, exec_state, invoke_ctx)),
        CallableType::Builtin {
            kind: BuiltinKind::BorrowingNative205(_, function, cost_function, cost_input),
            ..
        } => {
            let cost_input = if exec_state.epoch() >= &StacksEpochId::Epoch2_05 {
                cost_input(&args)
            } else {
                Ok(args.len() as u64)
            };
            cost_input
                .and_then(|cost_input| {
                    runtime_cost(cost_function.clone(), exec_state, cost_input)
                        .map_err(VmExecutionError::from)
                })
                .and_then(|()| function.apply(args, exec_state, invoke_ctx))
        }
        CallableType::UserFunction(function) => function.apply_refs(args, exec_state, invoke_ctx),
        _ => unreachable!("reference dispatch was classified above"),
    };
    add_stack_trace(&mut resp, exec_state);
    exec_state.drop_memory(used_memory)?;
    exec_state.call_stack.remove(&identifier, track_recursion)?;
    resp
}

/// Apply pre-evaluated sequence elements without charging another argument clone.
/// Legacy native/special consumers and active tracing retain their owned boundary.
pub fn apply_evaluated_refs(
    function: &CallableType,
    args: Vec<ValueRef<'_>>,
    exec_state: &mut ExecutionState,
    invoke_ctx: &InvocationContext,
    context: &LocalContext,
) -> Result<ValueRef<'static>, VmExecutionError> {
    let (identifier, track_recursion) = check_call_preconditions(function, exec_state)?;
    if !exec_state.has_eval_hooks() {
        if let CallableType::Builtin {
            clarity_name: name @ (">" | "<" | ">=" | "<="),
            kind: BuiltinKind::Special(..),
        } = function
        {
            exec_state.call_stack.insert(&identifier, track_recursion);
            let mut result = functions::apply_comparison_refs(name, &args, exec_state, invoke_ctx)
                .map(ValueRef::Owned);
            add_stack_trace(&mut result, exec_state);
            exec_state.call_stack.remove(&identifier, track_recursion)?;
            return result;
        }
    }
    let accepts_refs = matches!(
        function,
        CallableType::UserFunction(_)
            | CallableType::Builtin {
                kind: BuiltinKind::BorrowingNative(..) | BuiltinKind::BorrowingNative205(..),
                ..
            }
    );
    if !accepts_refs || exec_state.has_eval_hooks() {
        return apply_evaluated(
            function,
            args.into_iter()
                .map(ValueRef::into_owned)
                .collect::<Result<Vec<_>, _>>()?,
            exec_state,
            invoke_ctx,
            context,
        )
        .map(ValueRef::Owned);
    }
    // Pre-evaluated arguments historically arrived as owned Values. Remove lexical
    // clone provenance without a runtime charge before forwarding projected values.
    let args = args
        .into_iter()
        .map(ValueRef::into_evaluated)
        .collect::<Result<Vec<_>, _>>()?;
    let mut used_memory = 0;
    exec_state.call_stack.incr_apply_depth();
    let charged = (|| -> Result<(), VmExecutionError> {
        for arg in &args {
            let memory = arg.get_memory_use()?;
            exec_state.add_memory(memory)?;
            used_memory += memory;
        }
        Ok(())
    })();
    exec_state.call_stack.decr_apply_depth();
    if let Err(error) = charged {
        exec_state.drop_memory(used_memory)?;
        return Err(error);
    }
    dispatch_ref_args(
        function,
        identifier,
        track_recursion,
        args,
        used_memory,
        exec_state,
        invoke_ctx,
    )?
    .into_evaluated()
}

/// Evaluates unevaluated arguments and dispatches them to a [`CallableType`].
///
/// Each [`SymbolicExpression`] in `args` is evaluated (via [`eval`]) and charged for memory.
/// The resulting [`Value`]s are then dispatched through `dispatch_args` to the appropriate
/// callable variant (builtin or user-defined).
///
/// For [`BuiltinKind::Special`] functions, `args` are passed unevaluated — the special
/// function is responsible for evaluating its own arguments (e.g., short-circuiting in `and`/`or`).
///
/// Enforces recursion detection and max stack-depth limits before dispatch.
pub fn apply<'a>(
    function: &CallableType,
    args: &'a [SymbolicExpression],
    exec_state: &mut ExecutionState,
    invoke_ctx: &'a InvocationContext,
    context: &'a LocalContext,
) -> Result<ValueRef<'a>, VmExecutionError> {
    let (identifier, track_recursion) = check_call_preconditions(function, exec_state)?;
    let call_hook = CallTraceFrame::when(exec_state.has_eval_hooks(), || {
        function.call_trace_hook(invoke_ctx)
    });

    if let CallableType::Builtin {
        kind: BuiltinKind::Special(_, function),
        ..
    } = function
    {
        exec_state.call_stack.insert(&identifier, track_recursion);
        call_hook.begin(exec_state, invoke_ctx, CallArguments::Expressions(args));
        let mut resp = function(args, exec_state, invoke_ctx, context);
        call_hook.finish(exec_state, invoke_ctx, &resp);
        add_stack_trace(&mut resp, exec_state);
        exec_state.call_stack.remove(&identifier, track_recursion)?;
        return resp.map(ValueRef::Owned);
    }

    if let CallableType::Builtin {
        kind: BuiltinKind::StoredSpecial(_, function),
        ..
    } = function
    {
        exec_state.call_stack.insert(&identifier, track_recursion);
        call_hook.begin(exec_state, invoke_ctx, CallArguments::Expressions(args));
        let resp = function(args, exec_state, invoke_ctx, context);
        let mut resp = call_hook.finish_value_ref(exec_state, invoke_ctx, resp);
        add_stack_trace(&mut resp, exec_state);
        exec_state.call_stack.remove(&identifier, track_recursion)?;
        return resp;
    }

    let accepts_value_refs = matches!(
        function,
        CallableType::UserFunction(_)
            | CallableType::Builtin {
                kind: BuiltinKind::BorrowingNative(..) | BuiltinKind::BorrowingNative205(..),
                ..
            }
    );
    if accepts_value_refs {
        call_hook.begin(exec_state, invoke_ctx, CallArguments::Expressions(args));

        macro_rules! return_borrowing_error {
            ($err:expr, $used_memory:expr) => {{
                let error = match exec_state.drop_memory($used_memory) {
                    Ok(()) => $err,
                    Err(drop_error) => drop_error.into(),
                };
                exec_state.call_stack.decr_apply_depth();
                let mut result = call_hook.finish_value_ref(exec_state, invoke_ctx, Err(error));
                add_stack_trace(&mut result, exec_state);
                return result;
            }};
        }

        let mut used_memory = 0;
        let mut evaluated_args = Vec::with_capacity(args.len());
        exec_state.call_stack.incr_apply_depth();
        for (arg_index, arg) in args.iter().enumerate() {
            let value = match eval(arg, exec_state, invoke_ctx, context) {
                Ok(value) => value,
                Err(error) => return_borrowing_error!(error, used_memory),
            };
            if let Err(error) = value.charge_clone_cost(exec_state) {
                return_borrowing_error!(error, used_memory);
            }
            let memory = match value.get_memory_use() {
                Ok(memory) => memory,
                Err(error) => return_borrowing_error!(error, used_memory),
            };
            if let Err(error) = exec_state.add_memory(memory) {
                return_borrowing_error!(error.into(), used_memory);
            }
            used_memory += memory;
            call_hook.did_evaluate_value_ref(exec_state, invoke_ctx, arg_index, &value);
            evaluated_args.push(value);
        }
        exec_state.call_stack.decr_apply_depth();

        let resp = dispatch_ref_args(
            function,
            identifier,
            track_recursion,
            evaluated_args,
            used_memory,
            exec_state,
            invoke_ctx,
        );
        return call_hook.finish_value_ref(exec_state, invoke_ctx, resp);
    }

    call_hook.begin(exec_state, invoke_ctx, CallArguments::Expressions(args));

    macro_rules! return_call_error {
        ($err:expr) => {{
            let resp = Err($err);
            call_hook.finish(exec_state, invoke_ctx, &resp);
            return resp.map(ValueRef::Owned);
        }};
    }

    let mut used_memory = 0;
    let mut evaluated_args = Vec::with_capacity(args.len());
    exec_state.call_stack.incr_apply_depth();
    for (arg_index, arg_x) in args.iter().enumerate() {
        let arg_value = match eval(arg_x, exec_state, invoke_ctx, context)
            .and_then(|v| v.clone_with_cost(exec_state))
        {
            Ok(x) => x,
            Err(e) => {
                let err = match exec_state.drop_memory(used_memory) {
                    Ok(()) => e,
                    Err(drop_err) => drop_err.into(),
                };
                exec_state.call_stack.decr_apply_depth();
                return_call_error!(err);
            }
        };
        let arg_use = match arg_value.get_memory_use() {
            Ok(x) => x,
            Err(e) => {
                let err = match exec_state.drop_memory(used_memory) {
                    Ok(()) => e.into(),
                    Err(drop_err) => drop_err.into(),
                };
                exec_state.call_stack.decr_apply_depth();
                return_call_error!(err);
            }
        };
        match exec_state.add_memory(arg_use) {
            Ok(_x) => {}
            Err(e) => {
                let err = match exec_state.drop_memory(used_memory) {
                    Ok(()) => e.into(),
                    Err(drop_err) => drop_err.into(),
                };
                exec_state.call_stack.decr_apply_depth();
                return_call_error!(err);
            }
        };
        used_memory += arg_use;
        call_hook.did_evaluate_argument(exec_state, invoke_ctx, arg_index, &arg_value);
        evaluated_args.push(arg_value);
    }
    exec_state.call_stack.decr_apply_depth();

    let resp = dispatch_args(
        function,
        identifier,
        track_recursion,
        evaluated_args,
        used_memory,
        exec_state,
        invoke_ctx,
    );
    call_hook.finish(exec_state, invoke_ctx, &resp);
    resp.map(ValueRef::Owned)
}

/// Like [`apply`], but takes pre-evaluated [`Value`]s, skipping the `eval` + `clone_with_cost`
/// round-trip for every argument.
///
/// `fold`, `map`, and `filter` already have the element values as owned `Value`s; wrapping
/// them in `SymbolicExpression::atom_value` just to have `eval` clone them back out wastes N
/// allocations per step.  This function performs the same recursion/stack/memory bookkeeping
/// as `apply` while bypassing the eval pass entirely.
///
/// For [`BuiltinKind::Special`] functions (e.g. comparison operators `>=`, `<=`, `<`, `>`,
/// or boolean operators `and`, `or`), the values are wrapped back into
/// `SymbolicExpression::atom_value` so the special function can evaluate them normally
/// with `eval`.
pub fn apply_evaluated(
    function: &CallableType,
    args: Vec<Value>,
    exec_state: &mut ExecutionState,
    invoke_ctx: &InvocationContext,
    context: &LocalContext,
) -> Result<Value, VmExecutionError> {
    let (identifier, track_recursion) = check_call_preconditions(function, exec_state)?;
    let call_hook = CallTraceFrame::when(exec_state.has_eval_hooks(), || {
        function.call_trace_hook(invoke_ctx)
    });

    // `BuiltinKind::Special` functions require unevaluated SymbolicExpressions. They evaluate their own
    // arguments (e.g. short-circuit in `and`/`or`). Wrap the pre-evaluated Values back
    // into atom_value expressions so the special function dispatch works correctly.
    // This path is hit when built-in operators like >=, <=, <, >, and, or are used as
    // step functions in fold/map/filter. Note: In this case it works like `apply`.
    if let CallableType::Builtin {
        kind: BuiltinKind::Special(_, function),
        ..
    } = function
    {
        call_hook.begin(exec_state, invoke_ctx, CallArguments::Values(&args));
        call_hook.did_evaluate_arguments(exec_state, invoke_ctx, &args);
        let sym_args: Vec<SymbolicExpression> = args
            .into_iter()
            .map(SymbolicExpression::atom_value)
            .collect();
        exec_state.call_stack.insert(&identifier, track_recursion);
        let mut resp = function(&sym_args, exec_state, invoke_ctx, context);
        call_hook.finish(exec_state, invoke_ctx, &resp);
        add_stack_trace(&mut resp, exec_state);
        exec_state.call_stack.remove(&identifier, track_recursion)?;
        return resp;
    }

    if let CallableType::Builtin {
        kind: BuiltinKind::StoredSpecial(_, function),
        ..
    } = function
    {
        call_hook.begin(exec_state, invoke_ctx, CallArguments::Values(&args));
        call_hook.did_evaluate_arguments(exec_state, invoke_ctx, &args);
        let sym_args: Vec<SymbolicExpression> = args
            .into_iter()
            .map(SymbolicExpression::atom_value)
            .collect();
        exec_state.call_stack.insert(&identifier, track_recursion);
        let resp = function(&sym_args, exec_state, invoke_ctx, context);
        let mut resp = call_hook.finish_value_ref(exec_state, invoke_ctx, resp);
        add_stack_trace(&mut resp, exec_state);
        exec_state.call_stack.remove(&identifier, track_recursion)?;
        return resp?.clone_with_cost(exec_state);
    }

    call_hook.begin(exec_state, invoke_ctx, CallArguments::Values(&args));
    call_hook.did_evaluate_arguments(exec_state, invoke_ctx, &args);

    macro_rules! return_call_error {
        ($err:expr) => {{
            let resp = Err($err);
            call_hook.finish(exec_state, invoke_ctx, &resp);
            return resp;
        }};
    }

    let mut used_memory = 0;
    exec_state.call_stack.incr_apply_depth();
    for arg in args.iter() {
        let arg_use = match arg.get_memory_use() {
            Ok(x) => x,
            Err(e) => {
                let err = match exec_state.drop_memory(used_memory) {
                    Ok(()) => e.into(),
                    Err(drop_err) => drop_err.into(),
                };
                exec_state.call_stack.decr_apply_depth();
                return_call_error!(err);
            }
        };
        match exec_state.add_memory(arg_use) {
            Ok(_) => {}
            Err(e) => {
                let err = match exec_state.drop_memory(used_memory) {
                    Ok(()) => e.into(),
                    Err(drop_err) => drop_err.into(),
                };
                exec_state.call_stack.decr_apply_depth();
                return_call_error!(err);
            }
        };
        used_memory += arg_use;
    }
    exec_state.call_stack.decr_apply_depth();

    let resp = dispatch_args(
        function,
        identifier,
        track_recursion,
        args,
        used_memory,
        exec_state,
        invoke_ctx,
    );
    call_hook.finish(exec_state, invoke_ctx, &resp);
    resp
}

/// Check for interpreter-level violations of the resource limits
/// (execution time limit or excessive heap allocations).
fn check_interpreter_resource_usage(
    global_context: &GlobalContext,
) -> Result<(), VmExecutionError> {
    global_context
        .execution_resource_limiter
        .check_not_exceeded()
        .map_err(|err| match err {
            ResourceLimitExceeded::MaxDurationExceeded(s) => {
                RuntimeCheckErrorKind::ExecutionResourceBudgetExceeded(format!(
                    "Evaluation took too much time: {s}"
                ))
                .into()
            }
            ResourceLimitExceeded::MaxAllocationExceeded(s) => {
                RuntimeCheckErrorKind::ExecutionResourceBudgetExceeded(format!(
                    "Evaluation used too much memory: {s}"
                ))
                .into()
            }
        })
}

pub fn eval<'a>(
    exp: &'a SymbolicExpression,
    exec_state: &mut ExecutionState,
    invoke_ctx: &'a InvocationContext,
    context: &'a LocalContext,
) -> Result<ValueRef<'a>, VmExecutionError> {
    use crate::vm::representations::SymbolicExpressionType::{
        Atom, AtomValue, Field, List, LiteralValue, TraitReference,
    };

    check_interpreter_resource_usage(exec_state.global_context)?;

    exec_state.notify_will_begin_eval(invoke_ctx, context, exp);

    let res = match &exp.expr {
        AtomValue(value) | LiteralValue(value) => Ok(ValueRef::Owned(value.clone())),
        Atom(value) => lookup_variable(value, exec_state, invoke_ctx, context),
        List(children) => {
            let (function_variable, rest) =
                children
                    .split_first()
                    .ok_or(RuntimeCheckErrorKind::Unreachable(
                        "Non functional application".into(),
                    ))?;

            let function_name =
                function_variable
                    .match_atom()
                    .ok_or(RuntimeCheckErrorKind::Unreachable(
                        "Bad function name".into(),
                    ))?;
            let f = lookup_function(function_name, exec_state, invoke_ctx)?;
            apply(&f, rest, exec_state, invoke_ctx, context)
        }
        TraitReference(_, _) | Field(_) => {
            return Err(VmInternalError::BadSymbolicRepresentation(
                "Unexpected trait reference".into(),
            )
            .into());
        }
    };

    exec_state.notify_did_finish_eval(invoke_ctx, context, exp, &res);

    res
}

pub fn is_reserved(name: &str, version: &ClarityVersion) -> bool {
    functions::lookup_reserved_functions(name, version).is_some()
        || variables::is_reserved_name(name, version)
}

/// This function evaluates a list of expressions, sharing a global context.
/// It returns the final evaluated result.
/// Used for the initialization of a new contract.
pub fn eval_all(
    expressions: &[SymbolicExpression],
    contract_context: &mut ContractContext,
    global_context: &mut GlobalContext,
    sponsor: Option<PrincipalData>,
) -> Result<Option<Value>, VmExecutionError> {
    let mut last_executed = None;
    let context = LocalContext::new();
    let mut total_memory_use = 0;

    let publisher: PrincipalData = contract_context.contract_identifier.issuer.clone().into();

    finally_drop_memory!(global_context, total_memory_use; {
        for exp in expressions {
            let try_define = global_context.execute(|context| {
                let mut call_stack = CallStack::new();
                let mut exec_state = ExecutionState {
                    global_context: context,
                    call_stack: &mut call_stack,
                };
                let invoke_ctx = InvocationContext {
                    contract_context,
                    sender: Some(publisher.clone()),
                    caller: Some(publisher.clone()),
                    sponsor: sponsor.clone(),
                };
                functions::define::evaluate_define(exp, &mut exec_state, &invoke_ctx)
            })?;
            match try_define {
                DefineResult::Variable(name, value) => {
                    runtime_cost(ClarityCostFunction::BindName, global_context, 0)?;
                    let value_memory_use = value.get_memory_use()?;
                    global_context.add_memory(value_memory_use)?;
                    total_memory_use += value_memory_use;

                    contract_context.variables.insert(name, value);
                },
                DefineResult::Function(name, value) => {
                    runtime_cost(ClarityCostFunction::BindName, global_context, 0)?;

                    contract_context.functions.insert(name, value);
                },
                DefineResult::PersistedVariable(name, value_type, value) => {
                    runtime_cost(ClarityCostFunction::CreateVar, global_context, value_type.size()?)?;
                    contract_context.persisted_names.insert(name.clone());

                    global_context.add_memory(value_type.type_size()
                                              .map_err(|_| VmInternalError::Expect("Type size should be realizable".into()))?.into())?;

                    global_context.add_memory(value.size()?.into())?;

                    let data_type = global_context.database.create_variable(&contract_context.contract_identifier, &name, value_type)?;
                    global_context.database.set_variable(&contract_context.contract_identifier, &name, value, &data_type, &global_context.epoch_id)?;

                    contract_context.meta_data_var.insert(name, data_type);
                },
                DefineResult::Map(name, key_type, value_type) => {
                    runtime_cost(ClarityCostFunction::CreateMap, global_context,
                                  u64::from(key_type.size()?).cost_overflow_add(
                                      u64::from(value_type.size()?))?)?;
                    contract_context.persisted_names.insert(name.clone());

                    global_context.add_memory(key_type.type_size()
                                              .map_err(|_| VmInternalError::Expect("Type size should be realizable".into()))?.into())?;
                    global_context.add_memory(value_type.type_size()
                                              .map_err(|_| VmInternalError::Expect("Type size should be realizable".into()))?.into())?;

                    let data_type = global_context.database.create_map(&contract_context.contract_identifier, &name, key_type, value_type)?;

                    contract_context.meta_data_map.insert(name, data_type);
                },
                DefineResult::FungibleToken(name, total_supply) => {
                    runtime_cost(ClarityCostFunction::CreateFt, global_context, 0)?;
                    contract_context.persisted_names.insert(name.clone());

                    global_context.add_memory(TypeSignature::UIntType.type_size()
                                              .map_err(|_| VmInternalError::Expect("Type size should be realizable".into()))?.into())?;

                    let data_type = global_context.database.create_fungible_token(&contract_context.contract_identifier, &name, &total_supply)?;

                    contract_context.meta_ft.insert(name, data_type);
                },
                DefineResult::NonFungibleAsset(name, asset_type) => {
                    runtime_cost(ClarityCostFunction::CreateNft, global_context, asset_type.size()?)?;
                    contract_context.persisted_names.insert(name.clone());

                    global_context.add_memory(asset_type.type_size()
                                              .map_err(|_| VmInternalError::Expect("Type size should be realizable".into()))?.into())?;

                    let data_type = global_context.database.create_non_fungible_token(&contract_context.contract_identifier, &name, &asset_type)?;

                    contract_context.meta_nft.insert(name, data_type);
                },
                DefineResult::Trait(name, trait_type) => {
                    contract_context.defined_traits.insert(name, trait_type);
                },
                DefineResult::UseTrait(_name, _trait_identifier) => {},
                DefineResult::ImplTrait(trait_identifier) => {
                    contract_context.implemented_traits.insert(trait_identifier);
                },
                DefineResult::NoDefine => {
                    // not a define function, evaluate normally.
                    global_context.execute(|global_context| {
                        let mut call_stack = CallStack::new();
                        let mut exec_state = ExecutionState {
                            global_context,
                            call_stack: &mut call_stack,
                        };
                        let invoke_ctx = InvocationContext {
                            contract_context,
                            sender: Some(publisher.clone()),
                            caller: Some(publisher.clone()),
                            sponsor: sponsor.clone(),
                        };
                        let result = eval(exp, &mut exec_state, &invoke_ctx, &context)?.clone_with_cost(&mut exec_state)?;
                        last_executed = Some(result);
                        Ok(())
                    })?;
                }
            }
        }

        contract_context.data_size = total_memory_use;
        Ok(last_executed)
    })
}

/// Run provided program in a brand new environment, with a transient, empty
/// database. Only used for testing
/// This method executes the program in Epoch 2.0 *and* Epoch 2.05 and asserts
/// that the result is the same before returning the result
#[cfg(any(test, feature = "testing"))]
pub fn execute_on_network(
    program: &str,
    use_mainnet: bool,
) -> Result<Option<Value>, ClarityEvalError> {
    let epoch_200_result = execute_with_parameters(
        program,
        ClarityVersion::Clarity2,
        StacksEpochId::Epoch20,
        use_mainnet,
    );
    let epoch_205_result = execute_with_parameters(
        program,
        ClarityVersion::Clarity2,
        StacksEpochId::Epoch2_05,
        use_mainnet,
    );

    assert_eq!(
        epoch_200_result, epoch_205_result,
        "Epoch 2.0 and 2.05 should have same execution result, but did not for program `{program}`"
    );
    epoch_205_result
}

/// Runs `program` in a test environment with the provided parameters and calls
/// the provided functions before and after execution.
#[cfg(any(test, feature = "testing"))]
pub fn execute_with_parameters_and_call_in_global_context<F, G>(
    program: &str,
    clarity_version: ClarityVersion,
    epoch: StacksEpochId,
    use_mainnet: bool,
    sender: clarity_types::types::StandardPrincipalData,
    mut before_function: F,
    mut after_function: G,
) -> Result<Option<Value>, ClarityEvalError>
where
    F: FnMut(&mut GlobalContext) -> Result<(), VmExecutionError>,
    G: FnMut(&mut GlobalContext) -> Result<(), VmExecutionError>,
{
    use crate::vm::database::MemoryBackingStore;
    use crate::vm::tests::test_only_mainnet_to_chain_id;
    use crate::vm::types::QualifiedContractIdentifier;

    let contract_id =
        QualifiedContractIdentifier::new(sender, ContractName::from_literal("contract"));
    let mut contract_context = ContractContext::new(contract_id.clone(), clarity_version);
    let mut marf = MemoryBackingStore::new();
    let conn = marf.as_clarity_db();
    let chain_id = test_only_mainnet_to_chain_id(use_mainnet);
    let mut global_context = GlobalContext::new(
        use_mainnet,
        chain_id,
        conn,
        LimitedCostTracker::new_free(),
        epoch,
    );

    let parsed = ast::build_ast(
        &contract_id,
        program,
        &mut global_context.cost_track,
        clarity_version,
        epoch,
    )?
    .expressions;

    global_context
        .execute(|g| {
            before_function(g)?;
            let res = eval_all(&parsed, &mut contract_context, g, None);
            after_function(g)?;
            res
        })
        .map_err(ClarityEvalError::from)
}

#[cfg(any(test, feature = "testing"))]
pub fn execute_with_parameters(
    program: &str,
    clarity_version: ClarityVersion,
    epoch: StacksEpochId,
    use_mainnet: bool,
) -> Result<Option<Value>, ClarityEvalError> {
    execute_with_parameters_and_call_in_global_context(
        program,
        clarity_version,
        epoch,
        use_mainnet,
        clarity_types::types::StandardPrincipalData::transient(),
        |_| Ok(()),
        |_| Ok(()),
    )
}

/// Execute for test with `version`, Epoch20, testnet.
#[cfg(any(test, feature = "testing"))]
pub fn execute_against_version(
    program: &str,
    version: ClarityVersion,
) -> Result<Option<Value>, ClarityEvalError> {
    execute_with_parameters(program, version, StacksEpochId::Epoch20, false)
}

/// Execute for test in Clarity1, Epoch20, testnet.
#[cfg(any(test, feature = "testing"))]
pub fn execute(program: &str) -> Result<Option<Value>, ClarityEvalError> {
    execute_with_parameters(
        program,
        ClarityVersion::Clarity1,
        StacksEpochId::Epoch20,
        false,
    )
}

/// Execute for test in Clarity1, Epoch20, testnet.
#[cfg(any(test, feature = "testing"))]
pub fn execute_with_limited_execution_time(
    program: &str,
    max_execution_time: std::time::Duration,
) -> Result<Option<Value>, ClarityEvalError> {
    execute_with_parameters_and_call_in_global_context(
        program,
        ClarityVersion::Clarity1,
        StacksEpochId::Epoch20,
        false,
        clarity_types::types::StandardPrincipalData::transient(),
        |g| {
            let budget = ResourceBudget::new().with_max_duration(Some(max_execution_time));
            g.set_execution_resource_limiter(budget.start_tracking());
            Ok(())
        },
        |_| Ok(()),
    )
}

/// Execute for test in Clarity2, Epoch21, testnet.
#[cfg(any(test, feature = "testing"))]
pub fn execute_v2(program: &str) -> Result<Option<Value>, ClarityEvalError> {
    execute_with_parameters(
        program,
        ClarityVersion::Clarity2,
        StacksEpochId::Epoch21,
        false,
    )
}

/// Execute for test in Clarity6, Epoch40, testnet.
#[cfg(any(test, feature = "testing"))]
pub fn execute_v6(program: &str) -> Result<Option<Value>, ClarityEvalError> {
    execute_with_parameters(
        program,
        ClarityVersion::Clarity6,
        StacksEpochId::Epoch40,
        false,
    )
}

#[cfg(test)]
mod test {
    use clarity_types::ClarityName;
    use stacks_common::consts::CHAIN_ID_TESTNET;
    use stacks_common::types::StacksEpochId;

    use super::ClarityVersion;
    use crate::vm::callables::{DefineType, DefinedFunction};
    use crate::vm::contexts::{ExecutionState, InvocationContext};
    use crate::vm::costs::{CostErrors, CostTracker, ExecutionCost, LimitedCostTracker};
    use crate::vm::database::MemoryBackingStore;
    use crate::vm::types::codec::packed::{PackedValue, PackedValueVersion, SharedPackedValue};
    use crate::vm::types::{QualifiedContractIdentifier, TypeSignature};
    use crate::vm::{
        CallStack, ContractContext, GlobalContext, LocalContext, SymbolicExpression, Value,
        ValueCow, ValueRef, eval,
    };

    /// Records size inputs charged through the clone-cost compatibility path.
    #[derive(Default)]
    struct CloneCostTracker(Vec<u64>);

    impl CostTracker for CloneCostTracker {
        fn compute_cost(
            &mut self,
            function: crate::vm::costs::cost_functions::ClarityCostFunction,
            input: &[u64],
        ) -> Result<ExecutionCost, CostErrors> {
            assert_eq!(
                function,
                crate::vm::costs::cost_functions::ClarityCostFunction::LookupVariableSize
            );
            self.0.push(input[0]);
            Ok(ExecutionCost::ZERO)
        }

        fn add_cost(&mut self, _cost: ExecutionCost) -> Result<(), CostErrors> {
            Ok(())
        }

        fn add_memory(&mut self, _memory: u64) -> Result<(), CostErrors> {
            Ok(())
        }

        fn drop_memory(&mut self, _memory: u64) -> Result<(), CostErrors> {
            Ok(())
        }

        fn reset_memory(&mut self) {}
    }

    #[test]
    fn packed_local_binding_borrows_without_arc_churn() {
        let epoch = StacksEpochId::latest();
        let value = Value::buff_from(vec![7; 4_096]).unwrap();
        let expected = TypeSignature::type_of(&value).unwrap();
        let record = PackedValue::encode(PackedValueVersion::V1, &value).unwrap();
        let packed = SharedPackedValue::copy_from(record.as_bytes(), &expected, &epoch).unwrap();
        let stored = ValueRef::Packed(super::PackedValueCow::stored(packed.clone()));
        assert!(!stored.requires_clone_cost());
        let mut tracker = CloneCostTracker::default();
        stored.charge_clone_cost(&mut tracker).unwrap();
        assert!(tracker.0.is_empty());
        drop(stored);
        let binding = ValueCow::Packed(packed);

        let binding_ref = binding.as_value_ref();
        assert!(binding_ref.requires_clone_cost());
        binding_ref.charge_clone_cost(&mut tracker).unwrap();
        assert_eq!(tracker.0, vec![u64::from(expected.size().unwrap())]);
        assert_eq!(
            binding_ref.as_buffer_bytes().unwrap(),
            Some(&[7; 4_096][..])
        );
        let ValueCow::Packed(packed) = &binding else {
            unreachable!()
        };
        assert_eq!(packed.record_owner_count(), 1);
        assert!(!packed.is_materialized());
    }

    #[test]
    fn packed_utf8_element_projection_handles_multibyte_scalars() {
        let epoch = StacksEpochId::latest();
        let value = Value::string_utf8_from_bytes("hé".as_bytes().to_vec()).unwrap();
        let expected = TypeSignature::type_of(&value).unwrap();
        let record = PackedValue::encode(PackedValueVersion::V1, &value).unwrap();
        let packed = SharedPackedValue::copy_from(record.as_bytes(), &expected, &epoch).unwrap();

        let projected = ValueRef::Packed(super::PackedValueCow::stored(packed.clone()))
            .sequence_element_at(1)
            .unwrap()
            .unwrap();

        assert_eq!(
            projected,
            Value::string_utf8_from_bytes("é".as_bytes().to_vec()).unwrap()
        );
        assert!(!packed.is_materialized());
    }

    /// Packed projections preserve the clone charge of equivalent borrowed children.
    #[test]
    fn packed_binding_projections_preserve_clone_cost() {
        let epoch = StacksEpochId::latest();
        let fixtures = [
            Value::some(Value::UInt(7)).unwrap(),
            Value::okay(Value::UInt(7)).unwrap(),
            crate::vm::types::TupleData::from_data(vec![(
                ClarityName::from_literal("number"),
                Value::UInt(7),
            )])
            .unwrap()
            .into(),
        ];
        for (kind, value) in fixtures.into_iter().enumerate() {
            let expected = TypeSignature::type_of(&value).unwrap();
            let record = PackedValue::encode(PackedValueVersion::V1, &value).unwrap();
            let packed =
                SharedPackedValue::copy_from(record.as_bytes(), &expected, &epoch).unwrap();
            let owner = packed.clone();
            for borrowed in [false, true] {
                let bindings = [
                    ValueCow::Owned(value.clone()),
                    ValueCow::Packed(packed.clone()),
                ];
                let mut charges = Vec::new();
                for binding in &bindings {
                    let input = if borrowed {
                        binding.as_value_ref()
                    } else {
                        match binding {
                            ValueCow::Owned(value) => ValueRef::Owned(value.clone()),
                            ValueCow::Packed(value) => {
                                ValueRef::Packed(super::PackedValueCow::stored(value.clone()))
                            }
                        }
                    };
                    let child = match kind {
                        0 => input.optional_child().unwrap().unwrap(),
                        1 => input.response_child().unwrap().1,
                        _ => input.tuple_field("number").unwrap(),
                    };
                    let mut tracker = CloneCostTracker::default();
                    let result = child.into_static(&mut tracker).unwrap();
                    assert_eq!(result.as_uint().unwrap(), Some(7));
                    charges.push(tracker.0);
                }
                assert_eq!(
                    charges[0], charges[1],
                    "projection {kind}, borrowed={borrowed}"
                );
                assert_eq!(charges[0], if borrowed { vec![16] } else { vec![] });
                assert!(!owner.is_materialized());
            }
        }
    }

    /// Multiple retained children and synthetic optionals share the source payload bytes.
    #[test]
    fn packed_sequence_optionals_retain_child_addresses() {
        let epoch = StacksEpochId::latest();
        let values = vec![
            Value::buff_from(vec![7; 32768]).unwrap(),
            Value::buff_from(vec![9; 32768]).unwrap(),
        ];
        let list = Value::cons_list(values.clone(), &epoch).unwrap();
        let record = PackedValue::encode(PackedValueVersion::V1, &list).unwrap();
        let packed = SharedPackedValue::copy_from(
            record.as_bytes(),
            &TypeSignature::type_of(&list).unwrap(),
            &epoch,
        )
        .unwrap();
        let owner = ValueCow::Packed(packed.clone());
        let pointers: Vec<_> = (0..2)
            .map(|i| {
                packed
                    .as_view()
                    .as_list()
                    .unwrap()
                    .get(i)
                    .unwrap()
                    .unwrap()
                    .as_sequence_bytes()
                    .unwrap()
                    .as_ptr()
            })
            .collect();
        SharedPackedValue::reset_materialization_count();
        let first = owner
            .as_value_ref()
            .sequence_element_ref(0)
            .unwrap()
            .unwrap()
            .into_optional()
            .unwrap();
        let second = owner
            .as_value_ref()
            .sequence_element_ref(1)
            .unwrap()
            .unwrap()
            .into_optional()
            .unwrap()
            .into_optional()
            .unwrap();
        assert_eq!(
            first.size().unwrap(),
            Value::some(values[0].clone()).unwrap().size().unwrap()
        );
        assert_eq!(
            first.serialized_byte_len().unwrap(),
            Value::some(values[0].clone())
                .unwrap()
                .serialized_size()
                .unwrap()
        );
        drop(owner);
        drop(packed);
        let first = first.optional_child().unwrap().unwrap();
        let second = second
            .optional_child()
            .unwrap()
            .unwrap()
            .optional_child()
            .unwrap()
            .unwrap();
        assert_eq!(
            first.as_buffer_bytes().unwrap().unwrap().as_ptr(),
            pointers[0]
        );
        assert_eq!(
            second.as_buffer_bytes().unwrap().unwrap().as_ptr(),
            pointers[1]
        );
        assert_eq!(first.as_buffer_bytes().unwrap().unwrap(), &[7; 32768]);
        assert_eq!(second.as_buffer_bytes().unwrap().unwrap(), &[9; 32768]);
        assert_eq!(SharedPackedValue::materialization_count(), 0);
        assert_eq!(first.into_owned().unwrap(), values[0]);
        assert_eq!(SharedPackedValue::materialization_count(), 1);
    }

    #[test]
    fn test_simple_user_function() {
        //
        //  test program:
        //  (define (do_work x) (+ 5 x))
        //  (define a 59)
        //  (do_work a)
        //
        let content = [SymbolicExpression::list(vec![
            SymbolicExpression::atom(ClarityName::from_literal("do_work")),
            SymbolicExpression::atom(ClarityName::from_literal("a")),
        ])];

        let func_body = SymbolicExpression::list(vec![
            SymbolicExpression::atom(ClarityName::from_literal("+")),
            SymbolicExpression::atom_value(Value::Int(5)),
            SymbolicExpression::atom(ClarityName::from_literal("x")),
        ]);

        let func_args = vec![(ClarityName::from_literal("x"), TypeSignature::IntType)];
        let user_function = DefinedFunction::new(
            func_args,
            func_body,
            DefineType::Private,
            &ClarityName::from_literal("do_work"),
            "",
        );

        let context = LocalContext::new();
        let mut contract_context = ContractContext::new(
            QualifiedContractIdentifier::transient(),
            ClarityVersion::Clarity1,
        );

        let mut marf = MemoryBackingStore::new();
        let mut global_context = GlobalContext::new(
            false,
            CHAIN_ID_TESTNET,
            marf.as_clarity_db(),
            LimitedCostTracker::new_free(),
            StacksEpochId::Epoch2_05,
        );

        contract_context
            .variables
            .insert(ClarityName::from_literal("a"), Value::Int(59));
        contract_context
            .functions
            .insert(ClarityName::from_literal("do_work"), user_function);

        let mut call_stack = CallStack::new();
        let mut exec_state = ExecutionState {
            global_context: &mut global_context,
            call_stack: &mut call_stack,
        };
        let invoke_ctx = InvocationContext {
            contract_context: &contract_context,
            sender: None,
            caller: None,
            sponsor: None,
        };
        assert_eq!(
            Ok(ValueRef::Owned(Value::Int(64))),
            eval(&content[0], &mut exec_state, &invoke_ctx, &context)
        );
    }
}
