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

use std::{cmp, mem};

use stacks_common::bounded_format;
use stacks_common::types::StacksEpochId;

use crate::vm::contexts::{ExecutionState, InvocationContext};
use crate::vm::costs::cost_functions::ClarityCostFunction;
use crate::vm::costs::{CostOverflowingMath, runtime_cost};
use crate::vm::errors::{
    RuntimeCheckErrorKind, VmExecutionError, VmInternalError, check_argument_count,
    check_arguments_at_least,
};
use crate::vm::representations::SymbolicExpression;
use crate::vm::types::TypeSignature::BoolType;
use crate::vm::types::signatures::ListTypeData;
use crate::vm::types::{
    ASCIIData, BuffData, CharType, ListData, SequenceData, SequenceSubtype, StringSubtype,
    TypeSignature, UTF8Data, Value,
};
use crate::vm::{
    LocalContext, PackedValueCow, ValueCow, ValueRef, apply_evaluated_refs, eval, lookup_function,
    packed_vm_error,
};

pub fn list_cons(
    args: &[SymbolicExpression],
    exec_state: &mut ExecutionState,
    invoke_ctx: &InvocationContext,
    context: &LocalContext,
) -> Result<Value, VmExecutionError> {
    let eval_tried: Result<Vec<Value>, VmExecutionError> = args
        .iter()
        .map(|x| {
            eval(x, exec_state, invoke_ctx, context).and_then(|v| v.clone_with_cost(exec_state))
        })
        .collect();
    let args = eval_tried?;

    let mut arg_size = 0;
    for a in args.iter() {
        arg_size = arg_size.cost_overflow_add(a.size()?.into())?;
    }

    runtime_cost(ClarityCostFunction::ListCons, exec_state, arg_size)?;

    let value = Value::cons_list(args, exec_state.epoch())?;
    Ok(value)
}

/// Evaluate a sequence expression while retaining packed storage behind one stable owner.
fn eval_sequence_cow(
    expression: &SymbolicExpression,
    exec_state: &mut ExecutionState,
    invoke_ctx: &InvocationContext,
    context: &LocalContext,
) -> Result<(ValueCow, usize, TypeSignature), VmExecutionError> {
    let sequence = eval(expression, exec_state, invoke_ctx, context)?;
    sequence.charge_clone_cost(exec_state)?;
    let sequence_type = sequence.type_signature()?;
    let Some(length) = sequence.sequence_len()? else {
        return Err(RuntimeCheckErrorKind::Unreachable(bounded_format!(
            "Expected sequence: {sequence_type}"
        ))
        .into());
    };
    Ok((sequence.into_cow(), length, sequence_type))
}

/// Forward sequence cursor; UTF-8 keeps a byte offset instead of rescanning prefixes.
struct SequenceCursor<'a> {
    /// Owner retained by the caller for all projected elements.
    sequence: &'a ValueCow,
    /// Next logical element index.
    index: usize,
    /// Next byte boundary in packed UTF-8 text.
    utf8_offset: usize,
}

impl<'a> SequenceCursor<'a> {
    /// Start at the first element without decoding the sequence.
    fn new(sequence: &'a ValueCow) -> Self {
        Self {
            sequence,
            index: 0,
            utf8_offset: 0,
        }
    }

    /// Select the next element, preserving shared compound children.
    fn next(&mut self) -> Result<ValueRef<'a>, VmExecutionError> {
        if let ValueCow::Packed(packed) = self.sequence {
            if packed.segmented_sequence_len().is_none()
                && matches!(
                    packed.expected(),
                    TypeSignature::SequenceType(SequenceSubtype::StringType(StringSubtype::UTF8(
                        _
                    )))
                )
            {
                let bytes = packed.as_view().as_sequence_bytes().expect("UTF-8 type");
                let remaining = bytes
                    .get(self.utf8_offset..)
                    .filter(|bytes| !bytes.is_empty())
                    .ok_or_else(|| {
                        VmInternalError::Expect("UTF-8 cursor exceeded sequence length".into())
                    })?;
                // Admitted UTF-8: locate the following leading byte, inspecting only this scalar.
                let width = remaining
                    .iter()
                    .skip(1)
                    .position(|byte| byte & 0xc0 != 0x80)
                    .map_or(remaining.len(), |offset| offset + 1);
                let end = self.utf8_offset + width;
                let value = Value::string_utf8_from_bytes(bytes[self.utf8_offset..end].to_vec())?;
                self.utf8_offset = end;
                self.index += 1;
                return Ok(ValueRef::Owned(value));
            }
        }
        let element = self
            .sequence
            .as_value_ref()
            .sequence_element_ref(self.index)?
            .ok_or_else(|| {
                RuntimeCheckErrorKind::Unreachable(
                    "sequence element index is shorter than its reported length".into(),
                )
            })?;
        self.index += 1;
        Ok(element)
    }
}

/// Rebuild a filtered sequence while retaining its original maximum type bound.
fn filtered_sequence(
    sequence_type: TypeSignature,
    values: Vec<Value>,
) -> Result<Value, VmExecutionError> {
    let TypeSignature::SequenceType(sequence_type) = sequence_type else {
        return Err(VmInternalError::Expect("filtered value lost its sequence type".into()).into());
    };

    let value = match sequence_type {
        SequenceSubtype::BufferType(_) => {
            let mut data = Vec::with_capacity(values.len());
            for value in values {
                let Value::Sequence(SequenceData::Buffer(element)) = value else {
                    return Err(VmInternalError::Expect(
                        "buffer projection produced a non-buffer element".into(),
                    )
                    .into());
                };
                let [byte] = element.data.as_slice() else {
                    return Err(VmInternalError::Expect(
                        "buffer projection produced a non-unit element".into(),
                    )
                    .into());
                };
                data.push(*byte);
            }
            Value::Sequence(SequenceData::Buffer(BuffData { data }))
        }
        SequenceSubtype::StringType(StringSubtype::ASCII(_)) => {
            let mut data = Vec::with_capacity(values.len());
            for value in values {
                let Value::Sequence(SequenceData::String(CharType::ASCII(element))) = value else {
                    return Err(VmInternalError::Expect(
                        "ASCII projection produced a non-ASCII element".into(),
                    )
                    .into());
                };
                let [byte] = element.data.as_slice() else {
                    return Err(VmInternalError::Expect(
                        "ASCII projection produced a non-unit element".into(),
                    )
                    .into());
                };
                data.push(*byte);
            }
            Value::Sequence(SequenceData::String(CharType::ASCII(ASCIIData { data })))
        }
        SequenceSubtype::StringType(StringSubtype::UTF8(_)) => {
            let mut data = Vec::with_capacity(values.len());
            for value in values {
                let Value::Sequence(SequenceData::String(CharType::UTF8(mut element))) = value
                else {
                    return Err(VmInternalError::Expect(
                        "UTF-8 projection produced a non-UTF-8 element".into(),
                    )
                    .into());
                };
                let [character] = element.data.as_mut_slice() else {
                    return Err(VmInternalError::Expect(
                        "UTF-8 projection produced a non-unit element".into(),
                    )
                    .into());
                };
                data.push(mem::take(character));
            }
            Value::Sequence(SequenceData::String(CharType::UTF8(UTF8Data { data })))
        }
        SequenceSubtype::ListType(type_signature) => {
            Value::Sequence(SequenceData::List(ListData {
                data: values,
                type_signature,
            }))
        }
    };
    Ok(value)
}

/// Implements the Clarity `filter` function: `(filter func sequence)`.
///
/// Applies a boolean predicate `func` to each element of `sequence`, returning a new
/// sequence containing only the elements for which `func` returned `true`.
/// The predicate must return a `bool`; a type error is raised otherwise.
///
/// `args[0]` is the function name (atom) and `args[1]` is the sequence expression.
pub fn special_filter_ref(
    args: &[SymbolicExpression],
    exec_state: &mut ExecutionState,
    invoke_ctx: &InvocationContext,
    context: &LocalContext,
) -> Result<ValueRef<'static>, VmExecutionError> {
    check_argument_count(2, args)?;

    runtime_cost(ClarityCostFunction::Filter, exec_state, 0)?;

    let function_name = args[0]
        .match_atom()
        .ok_or(RuntimeCheckErrorKind::Unreachable("Expected name".into()))?;

    let (sequence, sequence_len, sequence_type) =
        eval_sequence_cow(&args[1], exec_state, invoke_ctx, context)?;
    let function = lookup_function(function_name, exec_state, invoke_ctx)?;

    let projected = matches!(
        (&sequence, &sequence_type),
        (
            ValueCow::Packed(_),
            TypeSignature::SequenceType(SequenceSubtype::ListType(_))
        )
    );
    let mut retained = Vec::with_capacity(if projected { 0 } else { sequence_len });
    let mut indices = Vec::new();
    let mut cursor = SequenceCursor::new(&sequence);
    for index in 0..sequence_len {
        let element = cursor.next()?;
        let element = element.into_cow();
        let filter_eval = apply_evaluated_refs(
            &function,
            vec![element.as_value_ref()],
            exec_state,
            invoke_ctx,
            context,
        )?;
        match filter_eval.as_bool()? {
            Some(true) => {
                if projected {
                    indices.push(index as u32);
                } else {
                    retained.push(element.as_value_ref().into_owned()?);
                }
            }
            Some(false) => {}
            _ => {
                return Err(RuntimeCheckErrorKind::TypeValueError(
                    Box::new(BoolType),
                    filter_eval.as_ref().to_error_string(),
                )
                .into());
            }
        }
    }
    if projected {
        let ValueCow::Packed(source) = sequence else {
            unreachable!("classified packed list");
        };
        return Ok(ValueRef::Packed(PackedValueCow::stored(
            source.filtered_list(indices).map_err(packed_vm_error)?,
        )));
    }
    filtered_sequence(sequence_type, retained).map(ValueRef::Owned)
}

/// Owned compatibility entry point for callers requiring a materialized filtered result.
pub fn special_filter(
    args: &[SymbolicExpression],
    exec_state: &mut ExecutionState,
    invoke_ctx: &InvocationContext,
    context: &LocalContext,
) -> Result<Value, VmExecutionError> {
    special_filter_ref(args, exec_state, invoke_ctx, context)?.into_owned()
}

/// Implements the Clarity `fold` function: `(fold func sequence initial)`.
///
/// Iterates over `sequence`, threading an accumulator through successive calls to `func`.
/// Each step calls `func` with `(element, accumulator)` and uses the result as the new
/// accumulator. Returns the final accumulator value.
///
/// `args[0]` is the function name (atom), `args[1]` is the sequence expression,
/// and `args[2]` is the initial accumulator value.
pub fn special_fold_ref(
    args: &[SymbolicExpression],
    exec_state: &mut ExecutionState,
    invoke_ctx: &InvocationContext,
    context: &LocalContext,
) -> Result<ValueRef<'static>, VmExecutionError> {
    check_argument_count(3, args)?;

    runtime_cost(ClarityCostFunction::Fold, exec_state, 0)?;

    let function_name = args[0]
        .match_atom()
        .ok_or(RuntimeCheckErrorKind::Unreachable("Expected name".into()))?;

    let function = lookup_function(function_name, exec_state, invoke_ctx)?;
    let (sequence, sequence_len, _) = eval_sequence_cow(&args[1], exec_state, invoke_ctx, context)?;
    let initial = eval(&args[2], exec_state, invoke_ctx, context)?.into_static(exec_state)?;

    let mut acc = initial;
    let mut cursor = SequenceCursor::new(&sequence);
    for _ in 0..sequence_len {
        let element = cursor.next()?;
        acc = apply_evaluated_refs(
            &function,
            vec![element, acc],
            exec_state,
            invoke_ctx,
            context,
        )?;
    }
    Ok(acc)
}

/// Owned compatibility entry point for callers outside reference dispatch.
pub fn special_fold(
    args: &[SymbolicExpression],
    exec_state: &mut ExecutionState,
    invoke_ctx: &InvocationContext,
    context: &LocalContext,
) -> Result<Value, VmExecutionError> {
    special_fold_ref(args, exec_state, invoke_ctx, context)?.into_owned()
}

/// Implements the Clarity `map` function: `(map func sequence-0 ... sequence-n)`.
///
/// Applies `func` element-wise across one or more input sequences, collecting the results
/// into a new list. When multiple sequences are provided, iteration stops at the length of
/// the shortest sequence. Each call to `func` receives one element from each sequence,
/// positionally (e.g., the i-th call gets the i-th element of every sequence).
///
/// `args[0]` is the function name (atom) and `args[1..]` are the sequence expressions.
///
/// # Epoch-gated dispatch
///
/// Dispatch keys on [`StacksEpochId::fixes_map_off_by_one`]:
/// - [`special_map_v200`]: before the fix (legacy re-arrange logic with an
///   off-by-one in the shortest-sequence bound)
/// - [`special_map_v400`]: from the fix onward (iterates exactly the length of
///   the shortest input sequence)
pub fn special_map_ref(
    args: &[SymbolicExpression],
    exec_state: &mut ExecutionState,
    invoke_ctx: &InvocationContext,
    context: &LocalContext,
) -> Result<ValueRef<'static>, VmExecutionError> {
    if exec_state.epoch().fixes_map_off_by_one() {
        special_map_v400_ref(args, exec_state, invoke_ctx, context)
    } else {
        special_map_v200_ref(args, exec_state, invoke_ctx, context)
    }
}

/// Legacy `map` implementation for Epoch [2.0 .. 3.4].
///
/// This re-arranges the input sequences into per-index argument tuples before
/// applying the function. It contains a known off-by-one in the shortest-
/// sequence bound (`apply_index > min_args_len` instead of `>=`), which is
/// preserved here for consensus compatibility. The fixed behavior lives in
/// [`special_map_v400`] and is gated to Epoch 4.0+.
pub fn special_map_v200_ref(
    args: &[SymbolicExpression],
    exec_state: &mut ExecutionState,
    invoke_ctx: &InvocationContext,
    context: &LocalContext,
) -> Result<ValueRef<'static>, VmExecutionError> {
    check_arguments_at_least(2, args)?;

    runtime_cost(ClarityCostFunction::Map, exec_state, args.len())?;

    let function_name = args[0]
        .match_atom()
        .ok_or(RuntimeCheckErrorKind::Unreachable("Expected name".into()))?;
    let function = lookup_function(function_name, exec_state, invoke_ctx)?;

    // Let's consider a function f (f a b c ...)
    // We will first re-arrange our sequences [a0, a1, ...] [b0, b1, ...] [c0, c1, ...] ...
    // To get something like: [a0, b0, c0, ...] [a1, b1, c1, ...]
    let mut mapped_func_args: Vec<Vec<ValueRef<'static>>> = vec![];
    let mut min_args_len = usize::MAX;
    for map_arg in args[1..].iter() {
        let (sequence, seq_len, _) = eval_sequence_cow(map_arg, exec_state, invoke_ctx, context)?;
        min_args_len = min_args_len.min(seq_len);
        let mut cursor = SequenceCursor::new(&sequence);
        for apply_index in 0..seq_len {
            let value = cursor.next()?.into_evaluated()?;
            if apply_index > min_args_len {
                break;
            }
            if apply_index >= mapped_func_args.len() {
                mapped_func_args.push(vec![value]);
            } else {
                mapped_func_args[apply_index].push(value);
            }
        }
    }

    // We can now apply the map
    let mut mapped_results = vec![];
    let mut previous_len = None;
    for arguments in mapped_func_args.into_iter() {
        // Stop iterating when we are done with the shortest sequence
        if let Some(previous_len) = previous_len {
            if previous_len != arguments.len() {
                break;
            }
        } else {
            previous_len = Some(arguments.len());
        }
        let res = apply_evaluated_refs(&function, arguments, exec_state, invoke_ctx, context)?;
        mapped_results.push(res);
    }

    let value = ValueRef::list_from(mapped_results, exec_state.epoch())?;
    Ok(value)
}

/// Fixed `map` implementation introduced in Clarity 6 (Epoch 4.0+).
///
/// Evaluates each sequence argument into an iterator, records its length, and
/// applies the function exactly `min_args_len` times — the length of the
/// shortest input sequence — pulling one element from each iterator per call.
/// This corrects the off-by-one in [`special_map_v200`].
pub fn special_map_v400_ref(
    args: &[SymbolicExpression],
    exec_state: &mut ExecutionState,
    invoke_ctx: &InvocationContext,
    context: &LocalContext,
) -> Result<ValueRef<'static>, VmExecutionError> {
    check_arguments_at_least(2, args)?;

    runtime_cost(ClarityCostFunction::Map, exec_state, args.len())?;

    let function_name = args[0]
        .match_atom()
        .ok_or(RuntimeCheckErrorKind::Unreachable("Expected name".into()))?;
    let function = lookup_function(function_name, exec_state, invoke_ctx)?;

    // Evaluate each sequence argument into an iterator and record its length.
    let mut sequences = Vec::with_capacity(args.len() - 1);
    let mut min_args_len = usize::MAX;
    for map_arg in args[1..].iter() {
        let (sequence, length, _) = eval_sequence_cow(map_arg, exec_state, invoke_ctx, context)?;
        min_args_len = min_args_len.min(length);
        sequences.push(sequence);
    }

    let mut cursors: Vec<_> = sequences.iter().map(SequenceCursor::new).collect();

    // Apply the function element-wise, stopping at the shortest sequence.
    let mut mapped_results = Vec::with_capacity(min_args_len);
    for _ in 0..min_args_len {
        let mut call_args = Vec::with_capacity(sequences.len());
        for cursor in &mut cursors {
            call_args.push(cursor.next()?);
        }
        let res = apply_evaluated_refs(&function, call_args, exec_state, invoke_ctx, context)?;
        mapped_results.push(res);
    }

    let value = ValueRef::list_from(mapped_results, exec_state.epoch())?;
    Ok(value)
}

pub fn special_append(
    args: &[SymbolicExpression],
    exec_state: &mut ExecutionState,
    invoke_ctx: &InvocationContext,
    context: &LocalContext,
) -> Result<Value, VmExecutionError> {
    check_argument_count(2, args)?;

    let sequence = eval(&args[0], exec_state, invoke_ctx, context)?.clone_with_cost(exec_state)?;
    match sequence {
        Value::Sequence(SequenceData::List(list)) => {
            let element = eval(&args[1], exec_state, invoke_ctx, context)?;
            let ListData {
                mut data,
                type_signature,
            } = list;
            let (entry_type, size) = type_signature.destruct();
            let element_type = element.type_signature()?;
            runtime_cost(
                ClarityCostFunction::Append,
                exec_state,
                u64::from(cmp::max(entry_type.size()?, element_type.size()?)),
            )?;
            let element = element.clone_with_cost(exec_state)?;
            if entry_type.is_no_type() {
                assert_eq!(size, 0);
                return Ok(Value::cons_list(vec![element], exec_state.epoch())?);
            }

            let next_entry_type =
                TypeSignature::least_supertype(exec_state.epoch(), &entry_type, &element_type)?;
            let (element, _) = Value::sanitize_value(exec_state.epoch(), &next_entry_type, element)
                .ok_or(RuntimeCheckErrorKind::ListTypesMustMatch)?;

            let next_type_signature = ListTypeData::new_list(next_entry_type, size + 1)?;
            data.push(element);
            Ok(Value::Sequence(SequenceData::List(ListData {
                type_signature: next_type_signature,
                data,
            })))
        }
        _ => Err(RuntimeCheckErrorKind::Unreachable("Expected list application".into()).into()),
    }
}

/// Epoch-based dispatch for `concat`.
///
/// - [`special_concat_v200`]: Epoch 2.0 (legacy size-based cost)
/// - [`special_concat_v205`]: Epoch 2.05 .. 3.4 (per-element cost, exactly 2 args)
/// - [`special_concat_v400`]: Epoch 4.0+ (Clarity 6 variadic `concat`)
pub fn special_concat(
    args: &[SymbolicExpression],
    exec_state: &mut ExecutionState,
    invoke_ctx: &InvocationContext,
    context: &LocalContext,
) -> Result<Value, VmExecutionError> {
    match exec_state.epoch() {
        StacksEpochId::Epoch10 => {
            panic!("Executing Clarity method during Epoch 1.0, before Clarity")
        }
        StacksEpochId::Epoch20 => special_concat_v200(args, exec_state, invoke_ctx, context),
        StacksEpochId::Epoch2_05
        | StacksEpochId::Epoch21
        | StacksEpochId::Epoch22
        | StacksEpochId::Epoch23
        | StacksEpochId::Epoch24
        | StacksEpochId::Epoch25
        | StacksEpochId::Epoch30
        | StacksEpochId::Epoch31
        | StacksEpochId::Epoch32
        | StacksEpochId::Epoch33
        | StacksEpochId::Epoch34 => special_concat_v205(args, exec_state, invoke_ctx, context),
        StacksEpochId::Epoch40 | StacksEpochId::Epoch41 => {
            special_concat_v400(args, exec_state, invoke_ctx, context)
        }
    }
}

pub fn special_concat_v200(
    args: &[SymbolicExpression],
    exec_state: &mut ExecutionState,
    invoke_ctx: &InvocationContext,
    context: &LocalContext,
) -> Result<Value, VmExecutionError> {
    check_argument_count(2, args)?;

    let mut wrapped_seq =
        eval(&args[0], exec_state, invoke_ctx, context)?.clone_with_cost(exec_state)?;
    let other_wrapped_seq =
        eval(&args[1], exec_state, invoke_ctx, context)?.clone_with_cost(exec_state)?;

    runtime_cost(
        ClarityCostFunction::Concat,
        exec_state,
        u64::from(wrapped_seq.size()?).cost_overflow_add(u64::from(other_wrapped_seq.size()?))?,
    )?;

    match (&mut wrapped_seq, other_wrapped_seq) {
        (Value::Sequence(seq), Value::Sequence(other_seq)) => {
            seq.concat(exec_state.epoch(), other_seq)?
        }
        (Value::Sequence(_), other_value) => {
            // The first value is a sequence, but the second is not
            return Err(RuntimeCheckErrorKind::Unreachable(bounded_format!(
                "Expected sequence: {}",
                TypeSignature::type_of(&other_value)?
            ))
            .into());
        }
        (value, _) => {
            // The first value is not a sequence (the other may not be as well, but just error on the first)
            return Err(RuntimeCheckErrorKind::Unreachable(bounded_format!(
                "Expected sequence: {}",
                TypeSignature::type_of(value)?
            ))
            .into());
        }
    };

    Ok(wrapped_seq)
}

pub fn special_concat_v205(
    args: &[SymbolicExpression],
    exec_state: &mut ExecutionState,
    invoke_ctx: &InvocationContext,
    context: &LocalContext,
) -> Result<Value, VmExecutionError> {
    check_argument_count(2, args)?;

    let mut wrapped_seq =
        eval(&args[0], exec_state, invoke_ctx, context)?.clone_with_cost(exec_state)?;
    let other_wrapped_seq =
        eval(&args[1], exec_state, invoke_ctx, context)?.clone_with_cost(exec_state)?;

    match (&mut wrapped_seq, other_wrapped_seq) {
        (Value::Sequence(seq), Value::Sequence(other_seq)) => {
            runtime_cost(
                ClarityCostFunction::Concat,
                exec_state,
                (seq.len() as u64).cost_overflow_add(other_seq.len() as u64)?,
            )?;

            seq.concat(exec_state.epoch(), other_seq)?
        }
        (Value::Sequence(seq_data), other_value) => {
            runtime_cost(ClarityCostFunction::Concat, exec_state, 1)?;
            return Err(RuntimeCheckErrorKind::TypeValueError(
                Box::new(seq_data.type_signature()?),
                other_value.to_error_string(),
            )
            .into());
        }
        _ => {
            runtime_cost(ClarityCostFunction::Concat, exec_state, 1)?;
            return Err(RuntimeCheckErrorKind::Unreachable(bounded_format!(
                "Expected sequence: {}",
                TypeSignature::type_of(&wrapped_seq)?,
            ))
            .into());
        }
    };

    Ok(wrapped_seq)
}

/// Variadic `concat` introduced in Clarity 6 (Epoch 4.0+).
///
/// Accepts 2 or more sequence arguments and concatenates them in a single
/// pass with linear cost. The 2-argument case is byte-for-byte equivalent
/// to v205.
///
/// # Cost model
///
/// This deliberately departs from the per-step cost charged by v205. With
/// the v205 formula applied per fold step, `(concat a b c d)` would charge
/// `len(a)+len(b)` then `len(a+b)+len(c)` then `len(a+b+c)+len(d)` — roughly
/// `O(N² · L)` in number of args. The runtime work is amortized linear, so
/// that pricing over-charges users for work the runtime doesn't actually do.
///
/// The variadic form charges `total_len` once, which is linear in the size
/// of the final sequence. Variadic `concat` is therefore strictly cheaper
/// than the equivalent nested-binary form — that's intentional: it reflects
/// the real work done and is one of the user-visible reasons to prefer the
/// variadic form.
///
/// # Algorithm
///
/// Phase 1 evaluates every arg into a `Vec<Value>` while summing the total
/// length. Phase 2 charges cost once, takes the first arg as the accumulator,
/// pre-reserves capacity to fit the final result, and appends the remaining
/// args. Pre-reservation means the underlying `Vec` does not reallocate as
/// we concat — exactly one allocation per `special_concat_v400` call.
///
/// Peak memory during phase 1 is bounded by the type checker's sequence-
/// length limits: every arg is ≤ `MAX_VALUE_SIZE`, and the type checker
/// also bounds the *combined* length to that same ceiling, so we hold at
/// most ~`MAX_VALUE_SIZE` bytes of args alive.
///
/// The type checker (`check_special_concat`) rejects variadic calls from
/// contracts at `ClarityVersion < Clarity6`, so at this epoch a Clarity 1-5
/// contract only ever reaches this function with exactly two arguments —
/// in which case the two-pass form collapses to the same work and cost as
/// v205.
pub fn special_concat_v400(
    args: &[SymbolicExpression],
    exec_state: &mut ExecutionState,
    invoke_ctx: &InvocationContext,
    context: &LocalContext,
) -> Result<Value, VmExecutionError> {
    check_arguments_at_least(2, args)?;

    // Phase 1: evaluate every arg, summing the total length. Bail with a
    // clean error on the first non-sequence — defensive only, since the
    // type checker already enforces this.
    let mut values: Vec<Value> = Vec::with_capacity(args.len());
    let mut total_len: u64 = 0;
    for arg in args {
        let value = eval(arg, exec_state, invoke_ctx, context)?.clone_with_cost(exec_state)?;
        match &value {
            Value::Sequence(seq) => {
                total_len = total_len.cost_overflow_add(seq.len() as u64)?;
            }
            non_seq => {
                runtime_cost(ClarityCostFunction::Concat, exec_state, 1)?;
                return Err(RuntimeCheckErrorKind::Unreachable(bounded_format!(
                    "Expected sequence: {}",
                    TypeSignature::type_of(non_seq)?
                ))
                .into());
            }
        }
        values.push(value);
    }

    // Phase 2: charge cost once (linear in total_len), pre-allocate, append.
    runtime_cost(ClarityCostFunction::Concat, exec_state, total_len)?;

    let mut values_iter = values.into_iter();
    let mut result = values_iter
        .next()
        .expect("arity ≥ 2 guarantees at least one value");

    if let Value::Sequence(seq) = &mut result {
        let already = seq.len() as u64;
        // saturating_sub: if `already == total_len` (a single empty append
        // chain) we just reserve 0.
        let extra = total_len.saturating_sub(already);
        seq.reserve(extra as usize);
    }

    for other in values_iter {
        match (&mut result, other) {
            (Value::Sequence(seq), Value::Sequence(other_seq)) => {
                seq.concat(exec_state.epoch(), other_seq)?;
            }
            (Value::Sequence(seq_data), other_value) => {
                return Err(RuntimeCheckErrorKind::TypeValueError(
                    Box::new(seq_data.type_signature()?),
                    other_value.to_error_string(),
                )
                .into());
            }
            _ => unreachable!("first arg was validated as a sequence in phase 1"),
        }
    }

    Ok(result)
}

pub fn special_as_max_len_ref(
    args: &[SymbolicExpression],
    exec_state: &mut ExecutionState,
    invoke_ctx: &InvocationContext,
    context: &LocalContext,
) -> Result<ValueRef<'static>, VmExecutionError> {
    check_argument_count(2, args)?;

    let sequence = eval(&args[0], exec_state, invoke_ctx, context)?;
    sequence.charge_clone_cost(exec_state)?;

    runtime_cost(ClarityCostFunction::AsMaxLen, exec_state, 0)?;

    if let Some(Value::UInt(expected_len)) = args[1].match_literal_value() {
        let Some(sequence_len) = sequence.sequence_len()? else {
            return Err(RuntimeCheckErrorKind::Unreachable(bounded_format!(
                "Expected sequence: {}",
                sequence.type_signature()?
            ))
            .into());
        };
        let sequence_len = sequence_len as u128;
        if sequence_len > *expected_len {
            Ok(ValueRef::Owned(Value::none()))
        } else {
            if let ValueRef::Packed(value) = sequence {
                let mut value = value.into_shared();
                if matches!(
                    value.expected(),
                    TypeSignature::SequenceType(SequenceSubtype::ListType(_))
                ) {
                    value = value
                        .with_list_bound(*expected_len as u32)
                        .map_err(crate::vm::composite_vm_error)?;
                }
                return ValueRef::from_shared(value).into_optional();
            }
            let mut sequence = sequence.into_owned()?;
            if let Value::Sequence(SequenceData::List(ref mut list)) = sequence {
                list.type_signature.reduce_max_len(*expected_len as u32);
            }
            Ok(ValueRef::Owned(Value::some(sequence)?))
        }
    } else {
        let actual_len = eval(&args[1], exec_state, invoke_ctx, context)?;
        Err(RuntimeCheckErrorKind::TypeError(
            Box::new(TypeSignature::UIntType),
            Box::new(actual_len.type_signature()?),
        )
        .into())
    }
}

pub fn native_len(sequence: Value) -> Result<Value, VmExecutionError> {
    match sequence {
        Value::Sequence(sequence_data) => Ok(Value::UInt(sequence_data.len() as u128)),
        _ => Err(RuntimeCheckErrorKind::Unreachable(bounded_format!(
            "Expected sequence: {}",
            TypeSignature::type_of(&sequence)?
        ))
        .into()),
    }
}

/// Return a sequence length without materializing packed storage.
pub fn native_len_ref(sequence: ValueRef<'_>) -> Result<ValueRef<'_>, VmExecutionError> {
    sequence
        .sequence_len()?
        .map(|len| ValueRef::Owned(Value::UInt(len as u128)))
        .ok_or_else(|| {
            RuntimeCheckErrorKind::Unreachable(bounded_format!(
                "Expected sequence: {}",
                sequence.as_ref()
            ))
            .into()
        })
}

pub fn native_index_of(sequence: Value, to_find: Value) -> Result<Value, VmExecutionError> {
    if let Value::Sequence(sequence_data) = sequence {
        match sequence_data.contains(to_find)? {
            Some(index) => Ok(Value::some(Value::UInt(index as u128))?),
            None => Ok(Value::none()),
        }
    } else {
        Err(RuntimeCheckErrorKind::Unreachable(bounded_format!(
            "Expected sequence: {}",
            TypeSignature::type_of(&sequence)?
        ))
        .into())
    }
}

/// Search a sequence without materializing its packed container.
pub fn native_index_of_ref<'value>(
    sequence: ValueRef<'value>,
    to_find: ValueRef<'value>,
) -> Result<ValueRef<'value>, VmExecutionError> {
    let sequence_type = sequence.type_signature()?;
    let Some(length) = sequence.sequence_len()? else {
        return Err(RuntimeCheckErrorKind::Unreachable(bounded_format!(
            "Expected sequence: {sequence_type}"
        ))
        .into());
    };
    let TypeSignature::SequenceType(sequence_type) = sequence_type else {
        unreachable!("sequence_len accepted a non-sequence type")
    };
    let expected = match sequence_type {
        SequenceSubtype::ListType(_) => None,
        SequenceSubtype::BufferType(_) => Some(TypeSignature::BUFFER_MIN),
        SequenceSubtype::StringType(StringSubtype::ASCII(_)) => {
            Some(TypeSignature::STRING_ASCII_MIN)
        }
        SequenceSubtype::StringType(StringSubtype::UTF8(_)) => Some(TypeSignature::STRING_UTF8_MIN),
    };
    let unit_compatible = if let Some(expected) = expected {
        let actual = to_find.type_signature()?;
        if mem::discriminant(&actual) != mem::discriminant(&expected) {
            return Err(RuntimeCheckErrorKind::TypeValueError(
                Box::new(expected),
                to_find.as_ref().to_error_string(),
            )
            .into());
        }
        // The sequence subtype, not its maximum length, determines needle compatibility.
        let same_kind = match (&expected, &actual) {
            (
                TypeSignature::SequenceType(SequenceSubtype::BufferType(_)),
                TypeSignature::SequenceType(SequenceSubtype::BufferType(_)),
            ) => true,
            (
                TypeSignature::SequenceType(SequenceSubtype::StringType(a)),
                TypeSignature::SequenceType(SequenceSubtype::StringType(b)),
            ) => mem::discriminant(a) == mem::discriminant(b),
            _ => false,
        };
        if !same_kind {
            return Err(RuntimeCheckErrorKind::TypeValueError(
                Box::new(expected),
                to_find.as_ref().to_error_string(),
            )
            .into());
        }
        to_find.sequence_len()? == Some(1)
    } else {
        true
    };
    if !unit_compatible {
        return Ok(ValueRef::Owned(Value::none()));
    }

    let sequence = sequence.into_cow();
    let mut cursor = SequenceCursor::new(&sequence);
    for index in 0..length {
        if cursor.next()?.value_eq(&to_find)? {
            return Ok(ValueRef::Owned(Value::some(Value::UInt(index as u128))?));
        }
    }
    Ok(ValueRef::Owned(Value::none()))
}

pub fn native_element_at(sequence: Value, index: Value) -> Result<Value, VmExecutionError> {
    let sequence_data = if let Value::Sequence(sequence_data) = sequence {
        sequence_data
    } else {
        return Err(RuntimeCheckErrorKind::Unreachable(bounded_format!(
            "Expected sequence: {}",
            TypeSignature::type_of(&sequence)?
        ))
        .into());
    };

    let index = if let Value::UInt(index_u128) = index {
        if let Ok(index_usize) = usize::try_from(index_u128) {
            index_usize
        } else {
            return Ok(Value::none());
        }
    } else {
        return Err(RuntimeCheckErrorKind::TypeValueError(
            Box::new(TypeSignature::UIntType),
            index.to_error_string(),
        )
        .into());
    };

    if let Some(result) = sequence_data.element_at(index).map_err(|_| {
        VmInternalError::Expect("Sequence data constructed with invalid data.".into())
    })? {
        Ok(Value::some(result)?)
    } else {
        Ok(Value::none())
    }
}

/// Retain a selected packed child behind an optional wrapper without copying its payload.
pub fn native_element_at_ref<'value>(
    sequence: ValueRef<'value>,
    index: ValueRef<'value>,
) -> Result<ValueRef<'value>, VmExecutionError> {
    let Some(index) = index.as_uint()? else {
        return Err(RuntimeCheckErrorKind::TypeValueError(
            Box::new(TypeSignature::UIntType),
            index.as_ref().to_error_string(),
        )
        .into());
    };
    let Ok(index) = usize::try_from(index) else {
        return Ok(ValueRef::Owned(Value::none()));
    };
    match sequence.sequence_element_ref(index)? {
        Some(value) => value.into_optional(),
        None => Ok(ValueRef::Owned(Value::none())),
    }
}

/// Retain a packed range, materializing only when legacy child sanitization requires it.
fn slice_sequence(
    sequence: ValueRef<'_>,
    epoch: &StacksEpochId,
    left: usize,
    right: usize,
) -> Result<ValueRef<'static>, VmExecutionError> {
    if let ValueRef::Packed(packed) = sequence {
        if let Some(projected) = packed
            .sliced_sequence(left, right)
            .map_err(packed_vm_error)?
        {
            return Ok(ValueRef::Packed(PackedValueCow::stored(projected)));
        }
        // Only heterogeneous list ranges can require the legacy sanitization boundary.
        let owner = ValueCow::Packed(packed.into_shared());
        let mut cursor = SequenceCursor::new(&owner);
        cursor.index = left;
        let values = (left..right)
            .map(|_| cursor.next()?.into_owned())
            .collect::<Result<Vec<_>, VmExecutionError>>()?;
        return Ok(ValueRef::Owned(Value::cons_list(values, epoch)?));
    }

    let Value::Sequence(sequence) = sequence.into_owned()? else {
        return Err(VmInternalError::Expect("slice requires a sequence".into()).into());
    };
    sequence
        .slice(epoch, left, right)
        .map(ValueRef::Owned)
        .map_err(VmExecutionError::from)
}

/// Executes the Clarity2 function `slice?`.
pub fn special_slice_ref(
    args: &[SymbolicExpression],
    exec_state: &mut ExecutionState,
    invoke_ctx: &InvocationContext,
    context: &LocalContext,
) -> Result<ValueRef<'static>, VmExecutionError> {
    check_argument_count(3, args)?;

    let seq = eval(&args[0], exec_state, invoke_ctx, context)?;
    seq.charge_clone_cost(exec_state)?;
    let left_position = eval(&args[1], exec_state, invoke_ctx, context)?;
    let right_position = eval(&args[2], exec_state, invoke_ctx, context)?;

    let sliced_seq_res: Result<ValueRef<'static>, VmExecutionError> = (|| {
        match (
            seq.sequence_len()?,
            left_position.as_uint()?,
            right_position.as_uint()?,
        ) {
            (Some(length), Some(left_position), Some(right_position)) => {
                let (left_position, right_position) =
                    match (u32::try_from(left_position), u32::try_from(right_position)) {
                        (Ok(left_position), Ok(right_position)) => (left_position, right_position),
                        _ => return Ok(ValueRef::Owned(Value::none())),
                    };

                // Perform bound checks. Not necessary to check if positions are less than 0 since the vars are unsigned.
                if left_position as usize >= length || right_position as usize > length {
                    return Ok(ValueRef::Owned(Value::none()));
                }
                if right_position < left_position {
                    return Ok(ValueRef::Owned(Value::none()));
                }

                let TypeSignature::SequenceType(sequence_type) = seq.type_signature()? else {
                    unreachable!("checked sequence");
                };
                runtime_cost(
                    ClarityCostFunction::Slice,
                    exec_state,
                    (right_position - left_position) * sequence_type.unit_type().size()?,
                )?;
                let seq_value = slice_sequence(
                    seq,
                    exec_state.epoch(),
                    left_position as usize,
                    right_position as usize,
                )?;
                seq_value.into_optional()
            }
            _ => Err(RuntimeCheckErrorKind::Unreachable("Bad type construction".into()).into()),
        }
    })();

    match sliced_seq_res {
        Ok(sliced_seq) => Ok(sliced_seq),
        Err(e) => {
            runtime_cost(ClarityCostFunction::Slice, exec_state, 0)?;
            Err(e)
        }
    }
}

/// Owned compatibility entry point for callers requiring a materialized slice.
pub fn special_slice(
    args: &[SymbolicExpression],
    exec_state: &mut ExecutionState,
    invoke_ctx: &InvocationContext,
    context: &LocalContext,
) -> Result<Value, VmExecutionError> {
    special_slice_ref(args, exec_state, invoke_ctx, context)?.into_owned()
}

pub fn special_replace_at(
    args: &[SymbolicExpression],
    exec_state: &mut ExecutionState,
    invoke_ctx: &InvocationContext,
    context: &LocalContext,
) -> Result<Value, VmExecutionError> {
    check_argument_count(3, args)?;

    let seq = eval(&args[0], exec_state, invoke_ctx, context)?;
    let seq_type = seq.type_signature()?;

    // runtime is the cost to copy over one element into its place
    runtime_cost(ClarityCostFunction::ReplaceAt, exec_state, seq_type.size()?)?;

    let expected_elem_type = if let TypeSignature::SequenceType(seq_subtype) = &seq_type {
        seq_subtype.unit_type()
    } else {
        return Err(RuntimeCheckErrorKind::Unreachable(bounded_format!(
            "Expected sequence: {seq_type}"
        ))
        .into());
    };
    let index_val = eval(&args[1], exec_state, invoke_ctx, context)?;
    let new_element = eval(&args[2], exec_state, invoke_ctx, context)?;

    if expected_elem_type != TypeSignature::NoType
        && !expected_elem_type.admits(exec_state.epoch(), new_element.as_ref())?
    {
        return Err(RuntimeCheckErrorKind::TypeValueError(
            Box::new(expected_elem_type),
            new_element.as_ref().to_error_string(),
        )
        .into());
    }

    let index = if let Value::UInt(index_u128) = index_val.as_ref() {
        if let Ok(index_usize) = usize::try_from(*index_u128) {
            index_usize
        } else {
            return Ok(Value::none());
        }
    } else {
        return Err(RuntimeCheckErrorKind::TypeValueError(
            Box::new(TypeSignature::UIntType),
            index_val.as_ref().to_error_string(),
        )
        .into());
    };

    let Value::Sequence(data) = seq.clone_with_cost(exec_state)? else {
        return Err(RuntimeCheckErrorKind::Unreachable(bounded_format!(
            "Expected sequence: {seq_type}"
        ))
        .into());
    };
    let seq_len = data.len();
    if index >= seq_len {
        return Ok(Value::none());
    }
    let new_element = new_element.clone_with_cost(exec_state)?;
    Ok(data.replace_at(exec_state.epoch(), index, new_element)?)
}

/// Materialize the shared result for legacy direct callers.
pub fn special_map(
    args: &[SymbolicExpression],
    exec: &mut ExecutionState,
    invoke: &InvocationContext,
    context: &LocalContext,
) -> Result<Value, VmExecutionError> {
    special_map_ref(args, exec, invoke, context)?.into_owned()
}

/// Materialize the shared result for legacy direct callers.
pub fn special_map_v200(
    args: &[SymbolicExpression],
    exec: &mut ExecutionState,
    invoke: &InvocationContext,
    context: &LocalContext,
) -> Result<Value, VmExecutionError> {
    special_map_v200_ref(args, exec, invoke, context)?.into_owned()
}

/// Materialize the shared result for legacy direct callers.
pub fn special_map_v400(
    args: &[SymbolicExpression],
    exec: &mut ExecutionState,
    invoke: &InvocationContext,
    context: &LocalContext,
) -> Result<Value, VmExecutionError> {
    special_map_v400_ref(args, exec, invoke, context)?.into_owned()
}

/// Materialize the shared bound-restricted result for legacy direct callers.
pub fn special_as_max_len(
    args: &[SymbolicExpression],
    exec: &mut ExecutionState,
    invoke: &InvocationContext,
    context: &LocalContext,
) -> Result<Value, VmExecutionError> {
    special_as_max_len_ref(args, exec, invoke, context)?.into_owned()
}
