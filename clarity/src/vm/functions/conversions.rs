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

use clarity_types::errors::ClarityTypeError;
use clarity_types::types::serialization::SerializationError;
use stacks_common::util::hash::to_hex;

use crate::vm::contexts::{ExecutionState, InvocationContext};
use crate::vm::costs::cost_functions::ClarityCostFunction;
use crate::vm::costs::runtime_cost;
use crate::vm::errors::{
    RuntimeCheckErrorKind, VmExecutionError, VmInternalError, check_argument_count,
};
use crate::vm::representations::SymbolicExpression;
use crate::vm::types::SequenceSubtype::BufferType;
use crate::vm::types::TypeSignature::SequenceType;
use crate::vm::types::{
    ASCIIData, BufferLength, CharType, SequenceData, SequenceSubtype, StringSubtype, TypeSignature,
    TypeSignatureExt as _, UTF8Data, Value,
};
use crate::vm::{LocalContext, ValueRef, eval};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EndianDirection {
    LittleEndian,
    BigEndian,
}

// The functions in this file support conversion from (buff 16) to either 1) int or 2) uint,
// from formats 1) big-endian and 2) little-endian.
//
// The function 'buff_to_int_generic' describes the logic common to these four functions.
// This is a generic function for conversion from a buffer to an int or uint. The four
// versions of Clarity function each call this, with different values for 'conversion_fn'.
//
// This function checks and parses the arguments, and calls 'conversion_fn' to do
// the specific form of conversion required.
pub fn buff_to_int_generic(
    value: Value,
    direction: EndianDirection,
    conversion_fn: fn([u8; 16]) -> Value,
) -> Result<Value, VmExecutionError> {
    match value {
        Value::Sequence(SequenceData::Buffer(ref sequence_data)) => {
            if sequence_data
                .len()
                .map_err(|_| VmInternalError::Expect("Data length should be valid".into()))?
                > BufferLength::try_from(16_u32)
                    .map_err(|_| VmInternalError::Expect("Failed to construct".into()))?
            {
                Err(RuntimeCheckErrorKind::TypeValueError(
                    Box::new(SequenceType(BufferType(
                        BufferLength::try_from(16_u32)
                            .map_err(|_| VmInternalError::Expect("Failed to construct".into()))?,
                    ))),
                    value.to_error_string(),
                )
                .into())
            } else {
                let mut transfer_buffer = [0u8; 16];
                let original_slice = sequence_data.as_slice();
                // 'conversion_fn' expects to receive a 16-byte buffer. If the input is little-endian, it should
                // be zero-padded on the right. If the input is big-endian, it should be zero-padded on the left.
                let offset = if direction == EndianDirection::LittleEndian {
                    0
                } else {
                    transfer_buffer.len() - original_slice.len()
                };
                for (from_index, _) in original_slice.iter().enumerate() {
                    let to_index = from_index + offset;
                    transfer_buffer[to_index] = original_slice[from_index];
                }
                let value = conversion_fn(transfer_buffer);
                Ok(value)
            }
        }
        _ => Err(RuntimeCheckErrorKind::TypeValueError(
            Box::new(SequenceType(BufferType(
                BufferLength::try_from(16_u32)
                    .map_err(|_| VmInternalError::Expect("Failed to construct".into()))?,
            ))),
            value.to_error_string(),
        )
        .into()),
    }
}

/// Convert borrowed buffer bytes to an integer without materializing their container.
fn buff_to_int_ref(
    value: ValueRef<'_>,
    direction: EndianDirection,
    conversion_fn: fn([u8; 16]) -> Value,
) -> Result<ValueRef<'_>, VmExecutionError> {
    let Some(bytes) = value.as_buffer_bytes()? else {
        return Err(RuntimeCheckErrorKind::TypeValueError(
            Box::new(SequenceType(BufferType(
                BufferLength::try_from(16_u32)
                    .map_err(|_| VmInternalError::Expect("Failed to construct".into()))?,
            ))),
            value.as_ref().to_error_string(),
        )
        .into());
    };
    if bytes.len() > 16 {
        return Err(RuntimeCheckErrorKind::TypeValueError(
            Box::new(SequenceType(BufferType(
                BufferLength::try_from(16_u32)
                    .map_err(|_| VmInternalError::Expect("Failed to construct".into()))?,
            ))),
            value.as_ref().to_error_string(),
        )
        .into());
    }

    let mut transfer = [0u8; 16];
    let offset = match direction {
        EndianDirection::LittleEndian => 0,
        EndianDirection::BigEndian => transfer.len() - bytes.len(),
    };
    transfer[offset..offset + bytes.len()].copy_from_slice(bytes);
    Ok(ValueRef::Owned(conversion_fn(transfer)))
}

pub fn native_buff_to_int_le(value: Value) -> Result<Value, VmExecutionError> {
    fn convert_to_int_le(buffer: [u8; 16]) -> Value {
        let value = i128::from_le_bytes(buffer);
        Value::Int(value)
    }
    buff_to_int_generic(value, EndianDirection::LittleEndian, convert_to_int_le)
}

pub fn native_buff_to_uint_le(value: Value) -> Result<Value, VmExecutionError> {
    fn convert_to_uint_le(buffer: [u8; 16]) -> Value {
        let value = u128::from_le_bytes(buffer);
        Value::UInt(value)
    }

    buff_to_int_generic(value, EndianDirection::LittleEndian, convert_to_uint_le)
}

pub fn native_buff_to_int_be(value: Value) -> Result<Value, VmExecutionError> {
    fn convert_to_int_be(buffer: [u8; 16]) -> Value {
        let value = i128::from_be_bytes(buffer);
        Value::Int(value)
    }
    buff_to_int_generic(value, EndianDirection::BigEndian, convert_to_int_be)
}

pub fn native_buff_to_uint_be(value: Value) -> Result<Value, VmExecutionError> {
    fn convert_to_uint_be(buffer: [u8; 16]) -> Value {
        let value = u128::from_be_bytes(buffer);
        Value::UInt(value)
    }
    buff_to_int_generic(value, EndianDirection::BigEndian, convert_to_uint_be)
}

/// Decode a little-endian signed integer from borrowed buffer bytes.
pub fn native_buff_to_int_le_ref(value: ValueRef<'_>) -> Result<ValueRef<'_>, VmExecutionError> {
    buff_to_int_ref(value, EndianDirection::LittleEndian, |bytes| {
        Value::Int(i128::from_le_bytes(bytes))
    })
}

/// Decode a little-endian unsigned integer from borrowed buffer bytes.
pub fn native_buff_to_uint_le_ref(value: ValueRef<'_>) -> Result<ValueRef<'_>, VmExecutionError> {
    buff_to_int_ref(value, EndianDirection::LittleEndian, |bytes| {
        Value::UInt(u128::from_le_bytes(bytes))
    })
}

/// Decode a big-endian signed integer from borrowed buffer bytes.
pub fn native_buff_to_int_be_ref(value: ValueRef<'_>) -> Result<ValueRef<'_>, VmExecutionError> {
    buff_to_int_ref(value, EndianDirection::BigEndian, |bytes| {
        Value::Int(i128::from_be_bytes(bytes))
    })
}

/// Decode a big-endian unsigned integer from borrowed buffer bytes.
pub fn native_buff_to_uint_be_ref(value: ValueRef<'_>) -> Result<ValueRef<'_>, VmExecutionError> {
    buff_to_int_ref(value, EndianDirection::BigEndian, |bytes| {
        Value::UInt(u128::from_be_bytes(bytes))
    })
}

// This method represents the unified logic between both "string to int" and "string to uint".
// 'value' is the input value to be converted.
// 'string_to_value_fn' is a function that takes in a Rust-langauge string, and should output
//   either a Int or UInt, depending on the desired result.
pub fn native_string_to_int_generic(
    value: Value,
    string_to_value_fn: fn(&str) -> Result<Value, RuntimeCheckErrorKind>,
) -> Result<Value, VmExecutionError> {
    match value {
        Value::Sequence(SequenceData::String(CharType::ASCII(ASCIIData { data }))) => {
            match String::from_utf8(data) {
                Ok(as_string) => Ok(string_to_value_fn(&as_string)?),
                Err(_error) => Ok(Value::none()),
            }
        }
        Value::Sequence(SequenceData::String(CharType::UTF8(UTF8Data { data }))) => {
            let flattened_bytes = data.into_iter().flatten().collect();
            match String::from_utf8(flattened_bytes) {
                Ok(as_string) => Ok(string_to_value_fn(&as_string)?),
                Err(_error) => Ok(Value::none()),
            }
        }
        _ => Err(RuntimeCheckErrorKind::UnionTypeValueError(
            vec![
                TypeSignature::STRING_ASCII_MAX,
                TypeSignature::STRING_UTF8_MAX,
            ],
            value.to_error_string(),
        )
        .into()),
    }
}

fn safe_convert_string_to_int(raw_string: &str) -> Result<Value, RuntimeCheckErrorKind> {
    let possible_int = raw_string.parse::<i128>();
    match possible_int {
        Ok(val) => Ok(Value::some(Value::Int(val))?),
        Err(_error) => Ok(Value::none()),
    }
}

pub fn native_string_to_int(value: Value) -> Result<Value, VmExecutionError> {
    native_string_to_int_generic(value, safe_convert_string_to_int)
}

fn safe_convert_string_to_uint(raw_string: &str) -> Result<Value, RuntimeCheckErrorKind> {
    let possible_int = raw_string.parse::<u128>();
    match possible_int {
        Ok(val) => Ok(Value::some(Value::UInt(val))?),
        Err(_error) => Ok(Value::none()),
    }
}

pub fn native_string_to_uint(value: Value) -> Result<Value, VmExecutionError> {
    native_string_to_int_generic(value, safe_convert_string_to_uint)
}

/// Parse decimal sign and magnitude with the same grammar and overflow behavior as Rust integers.
fn decimal_magnitude(bytes: &[u8], signed: bool) -> Option<(bool, u128)> {
    let (negative, digits) = match bytes.split_first()? {
        (b'+', digits) => (false, digits),
        (b'-', digits) if signed => (true, digits),
        _ => (false, bytes),
    };
    if digits.is_empty() {
        return None;
    }
    let mut magnitude = 0u128;
    for byte in digits {
        let digit = byte.checked_sub(b'0')?;
        if digit > 9 {
            return None;
        }
        magnitude = magnitude.checked_mul(10)?.checked_add(u128::from(digit))?;
    }
    Some((negative, magnitude))
}

/// Convert borrowed decimal bytes directly to their optional Clarity integer.
fn decimal_value(bytes: &[u8], signed: bool) -> Option<Value> {
    let (negative, magnitude) = decimal_magnitude(bytes, signed)?;
    if !signed {
        return Some(Value::UInt(magnitude));
    }
    if negative && magnitude == 1u128 << 127 {
        return Some(Value::Int(i128::MIN));
    }
    let value = i128::try_from(magnitude).ok()?;
    Some(Value::Int(if negative { -value } else { value }))
}

/// Parse packed or owned text without validating or allocating a Rust string first.
fn string_to_int_ref(value: ValueRef<'_>, signed: bool) -> Result<ValueRef<'_>, VmExecutionError> {
    let Some(bytes) = value.as_text_bytes()? else {
        return Err(RuntimeCheckErrorKind::UnionTypeValueError(
            vec![
                TypeSignature::STRING_ASCII_MAX,
                TypeSignature::STRING_UTF8_MAX,
            ],
            value.as_ref().to_error_string(),
        )
        .into());
    };
    Ok(ValueRef::Owned(match decimal_value(&bytes, signed) {
        Some(value) => Value::some(value)?,
        None => Value::none(),
    }))
}

/// Parse a signed integer from borrowed packed or owned text.
pub fn native_string_to_int_ref(value: ValueRef<'_>) -> Result<ValueRef<'_>, VmExecutionError> {
    string_to_int_ref(value, true)
}

/// Parse an unsigned integer from borrowed packed or owned text.
pub fn native_string_to_uint_ref(value: ValueRef<'_>) -> Result<ValueRef<'_>, VmExecutionError> {
    string_to_int_ref(value, false)
}

// This method represents the unified logic between both "int to ascii" and "int to utf8".
// 'value' is the input value to be converted.
// 'bytes_to_value_fn' is a function that takes in a Rust-langauge byte sequence, and outputs
//   either an ASCII or UTF8 string, depending on the desired result.
pub fn native_int_to_string_generic(
    value: Value,
    bytes_to_value_fn: fn(bytes: Vec<u8>) -> Result<Value, ClarityTypeError>,
) -> Result<Value, VmExecutionError> {
    match value {
        Value::Int(ref int_value) => {
            let as_string = int_value.to_string();
            Ok(bytes_to_value_fn(as_string.into()).map_err(|_| {
                VmInternalError::Expect("Unexpected error converting Int to string.".into())
            })?)
        }
        Value::UInt(ref uint_value) => {
            let as_string = uint_value.to_string();
            Ok(bytes_to_value_fn(as_string.into()).map_err(|_| {
                VmInternalError::Expect("Unexpected error converting UInt to string.".into())
            })?)
        }
        _ => Err(RuntimeCheckErrorKind::UnionTypeValueError(
            vec![TypeSignature::IntType, TypeSignature::UIntType],
            value.to_error_string(),
        )
        .into()),
    }
}

pub fn native_int_to_ascii(value: Value) -> Result<Value, VmExecutionError> {
    // Given an integer, convert this to Clarity ASCII value.
    native_int_to_string_generic(value, Value::string_ascii_from_bytes)
}

pub fn native_int_to_utf8(value: Value) -> Result<Value, VmExecutionError> {
    // Given an integer, convert this to Clarity UTF8 value.
    native_int_to_string_generic(value, Value::string_utf8_from_bytes)
}

/// Convert a borrowed signed or unsigned integer to an ASCII string.
pub fn native_int_to_ascii_ref(value: ValueRef<'_>) -> Result<ValueRef<'_>, VmExecutionError> {
    let text = if let Some(value) = value.as_int()? {
        value.to_string()
    } else if let Some(value) = value.as_uint()? {
        value.to_string()
    } else {
        return Err(RuntimeCheckErrorKind::UnionTypeValueError(
            vec![TypeSignature::IntType, TypeSignature::UIntType],
            value.as_ref().to_error_string(),
        )
        .into());
    };
    Ok(ValueRef::Owned(Value::string_ascii_from_bytes(
        text.into_bytes(),
    )?))
}

/// Convert a borrowed signed or unsigned integer to a UTF-8 string.
pub fn native_int_to_utf8_ref(value: ValueRef<'_>) -> Result<ValueRef<'_>, VmExecutionError> {
    let text = if let Some(value) = value.as_int()? {
        value.to_string()
    } else if let Some(value) = value.as_uint()? {
        value.to_string()
    } else {
        return Err(RuntimeCheckErrorKind::UnionTypeValueError(
            vec![TypeSignature::IntType, TypeSignature::UIntType],
            value.as_ref().to_error_string(),
        )
        .into());
    };
    Ok(ValueRef::Owned(Value::string_utf8_from_bytes(
        text.into_bytes(),
    )?))
}

/// Helper function to convert a string to ASCII and wrap in Ok response
/// This should only fail due to system errors, not conversion failures
fn convert_string_to_ascii_ok(s: String) -> Result<Value, VmExecutionError> {
    let ascii_value = Value::string_ascii_from_bytes(s.into_bytes()).map_err(|_| {
        VmInternalError::Expect("Unexpected error converting string to ASCII".into())
    })?;
    Ok(Value::okay(ascii_value)?)
}

pub fn special_to_ascii(
    args: &[SymbolicExpression],
    exec_state: &mut ExecutionState,
    invoke_ctx: &InvocationContext,
    context: &LocalContext,
) -> Result<Value, VmExecutionError> {
    check_argument_count(1, args)?;

    let value = eval(&args[0], exec_state, invoke_ctx, context)?;

    runtime_cost(ClarityCostFunction::ToAscii, exec_state, value.size()?)?;

    if let Some(value) = value.as_int()? {
        return convert_string_to_ascii_ok(value.to_string());
    }
    if let Some(value) = value.as_uint()? {
        return convert_string_to_ascii_ok(format!("u{value}"));
    }
    if let Some(value) = value.as_bool()? {
        return convert_string_to_ascii_ok(value.to_string());
    }
    if let Some(bytes) = value.as_buffer_bytes()? {
        return convert_string_to_ascii_ok(format!("0x{}", to_hex(bytes)));
    }
    if matches!(
        value.type_signature()?,
        TypeSignature::SequenceType(SequenceSubtype::StringType(StringSubtype::UTF8(_)))
    ) {
        let bytes = value.as_text_bytes()?.expect("UTF-8 type");
        return if bytes.is_ascii() {
            Ok(Value::okay(Value::string_ascii_from_bytes(
                bytes.into_owned(),
            )?)?)
        } else {
            Ok(Value::err_uint(1))
        };
    }
    match value.as_ref() {
        Value::Principal(principal) => convert_string_to_ascii_ok(principal.to_string()),
        _ => Err(RuntimeCheckErrorKind::UnionTypeValueError(
            vec![
                TypeSignature::IntType,
                TypeSignature::UIntType,
                TypeSignature::BoolType,
                TypeSignature::PrincipalType,
                TypeSignature::TO_ASCII_BUFFER_MAX,
                TypeSignature::STRING_UTF8_MAX,
            ],
            value.as_ref().to_error_string(),
        )
        .into()),
    }
}

/// Returns `value` consensus serialized into a `(optional buff)` object.
/// If the value cannot fit as serialized into the maximum buffer size,
/// this returns `none`, otherwise, it will be `(some consensus-serialized-buffer)`
pub fn to_consensus_buff(value: Value) -> Result<Value, VmExecutionError> {
    let mut clar_buff_serialized = vec![];
    value
        .serialize_write(&mut clar_buff_serialized)
        .map_err(|_| VmInternalError::Expect("FATAL: failed to serialize to vec".into()))?;

    let clar_buff_serialized = match Value::buff_from(clar_buff_serialized) {
        Ok(x) => x,
        Err(_) => return Ok(Value::none()),
    };

    match Value::some(clar_buff_serialized) {
        Ok(x) => Ok(x),
        Err(_) => Ok(Value::none()),
    }
}

/// Deserialize a Clarity value from a consensus serialized buffer.
/// If the supplied buffer either fails to deserialize or deserializes
/// to an unexpected type, returns `none`. Otherwise, it will be `(some value)`
pub fn from_consensus_buff(
    args: &[SymbolicExpression],
    exec_state: &mut ExecutionState,
    invoke_ctx: &InvocationContext,
    context: &LocalContext,
) -> Result<Value, VmExecutionError> {
    check_argument_count(2, args)?;

    let type_arg = TypeSignature::parse_type_repr(*exec_state.epoch(), &args[0], exec_state)?;
    let value = eval(&args[1], exec_state, invoke_ctx, context)?;

    // get the buffer bytes from the supplied value. if not passed a buffer,
    // this is a type error
    let input_bytes = value.as_buffer_bytes()?.ok_or_else(|| {
        RuntimeCheckErrorKind::TypeValueError(
            Box::new(TypeSignature::BUFFER_MAX),
            value.as_ref().to_error_string(),
        )
    })?;

    let input = if invoke_ctx
        .contract_context
        .get_clarity_version()
        .protects_logn_cost_fn()
    {
        input_bytes.len().max(1)
    } else {
        input_bytes.len()
    };
    runtime_cost(ClarityCostFunction::FromConsensusBuff, exec_state, input)?;

    // Perform the deserialization and check that it deserialized to the expected
    // type. A type mismatch at this point is an error that should be surfaced in
    // Clarity (as a none return).
    let result = match Value::try_deserialize_bytes_exact_at_epoch(
        input_bytes,
        &type_arg,
        exec_state.epoch(),
    ) {
        Ok(value) => value,
        Err(SerializationError::UnexpectedSerialization) => {
            if exec_state.epoch().treats_unexpected_serialization_as_none() {
                return Ok(Value::none());
            }
            return Err(
                RuntimeCheckErrorKind::Unreachable("UnexpectedSerialization".into()).into(),
            );
        }
        Err(_) => return Ok(Value::none()),
    };
    if !type_arg.admits(exec_state.epoch(), &result)? {
        return Ok(Value::none());
    }

    Ok(Value::some(result)?)
}

#[cfg(test)]
mod packed_decimal_tests {
    use super::{decimal_value, native_string_to_int_ref, native_string_to_uint_ref};
    use crate::vm::types::codec::packed::{PackedValue, PackedValueVersion, SharedPackedValue};
    use crate::vm::types::{TypeSignature, Value};
    use crate::vm::{PackedValueCow, ValueRef};
    use proptest::prelude::*;
    use stacks_common::types::StacksEpochId;

    /// Compare the byte parser with the historical standard-library parser.
    fn check(text: &str) {
        assert_eq!(
            decimal_value(text.as_bytes(), true),
            text.parse::<i128>().ok().map(Value::Int),
            "signed {text:?}"
        );
        assert_eq!(
            decimal_value(text.as_bytes(), false),
            text.parse::<u128>().ok().map(Value::UInt),
            "unsigned {text:?}"
        );
    }

    /// Limits, signs, zeros and invalid Unicode preserve the exact historical conversion result.
    #[test]
    fn decimal_edges_match_standard_parser() {
        for text in [
            "",
            "+",
            "-",
            "-0",
            "+0",
            "0000",
            "+01",
            " 1",
            "1 ",
            "1_0",
            "１２",
            "é",
            "--1",
            "+-1",
            "1\0",
            "170141183460469231731687303715884105727",
            "170141183460469231731687303715884105728",
            "-170141183460469231731687303715884105728",
            "-170141183460469231731687303715884105729",
            "340282366920938463463374607431768211455",
            "340282366920938463463374607431768211456",
        ] {
            check(text);
            let value = Value::string_utf8_from_bytes(text.as_bytes().to_vec()).unwrap();
            let encoded = PackedValue::encode(PackedValueVersion::V1, &value).unwrap();
            let shared = SharedPackedValue::copy_from(
                encoded.as_bytes(),
                &TypeSignature::type_of(&value).unwrap(),
                &StacksEpochId::latest(),
            )
            .unwrap();
            let signed =
                native_string_to_int_ref(ValueRef::Packed(PackedValueCow::stored(shared.clone())))
                    .unwrap();
            let unsigned =
                native_string_to_uint_ref(ValueRef::Packed(PackedValueCow::stored(shared)))
                    .unwrap();
            assert_eq!(
                signed.as_ref(),
                &text
                    .parse::<i128>()
                    .ok()
                    .map_or_else(Value::none, |v| Value::some(Value::Int(v)).unwrap())
            );
            assert_eq!(
                unsigned.as_ref(),
                &text
                    .parse::<u128>()
                    .ok()
                    .map_or_else(Value::none, |v| Value::some(Value::UInt(v)).unwrap())
            );
        }
        check(&format!("{}1", "0".repeat(1000)));
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(2048))]
        #[test]
        fn arbitrary_text_matches_standard(text in ".{0,100}") { check(&text); }
        #[test]
        fn numeric_text_matches_standard(text in "[+\\-]?[0-9]{0,45}") { check(&text); }
        #[test]
        fn all_integer_values_match_standard(value in any::<i128>(), unsigned in any::<u128>()) {
            check(&value.to_string()); check(&unsigned.to_string());
        }
    }
}
