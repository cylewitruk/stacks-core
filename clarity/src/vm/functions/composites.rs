//! Constructors and updates that retain shared children through VM composition.

use clarity_types::types::MAX_UTF8_VALUE_SIZE;
use stacks_common::bounded_format;
use stacks_common::types::StacksEpochId;
use std::cmp;

use crate::vm::contexts::{ExecutionState, InvocationContext};
use crate::vm::costs::cost_functions::ClarityCostFunction;
use crate::vm::costs::{CostOverflowingMath, runtime_cost};
use crate::vm::errors::{
    RuntimeCheckErrorKind, SyntaxBindingErrorType, VmExecutionError, VmInternalError,
    check_argument_count, check_arguments_at_least,
};
use crate::vm::types::codec::packed::SharedPackedValue;
use crate::vm::types::{
    ListTypeData, MAX_VALUE_SIZE, SequenceData, SequenceSubtype, StringSubtype, TupleData,
    TypeSignature, Value,
};
use crate::vm::{LocalContext, SymbolicExpression, ValueRef, composite_vm_error, eval};

/// Build a list while preserving every borrowed child and historical evaluation costs.
pub fn list_cons(
    args: &[SymbolicExpression],
    exec: &mut ExecutionState,
    invoke: &InvocationContext,
    context: &LocalContext,
) -> Result<ValueRef<'static>, VmExecutionError> {
    let mut values = Vec::with_capacity(args.len());
    for arg in args {
        values.push(eval(arg, exec, invoke, context)?.into_static(exec)?);
    }
    let mut size = 0u64;
    for value in &values {
        size = size.cost_overflow_add(u64::from(value.size()?))?;
    }
    runtime_cost(ClarityCostFunction::ListCons, exec, size)?;
    ValueRef::list_from(values, exec.epoch())
}

/// Assemble tuple fields without materializing retained child payloads.
pub fn tuple_cons(
    args: &[SymbolicExpression],
    exec: &mut ExecutionState,
    invoke: &InvocationContext,
    context: &LocalContext,
) -> Result<ValueRef<'static>, VmExecutionError> {
    check_arguments_at_least(1, args)?;
    let mut fields = Vec::with_capacity(args.len());
    super::handle_binding_list::<_, VmExecutionError>(
        args,
        SyntaxBindingErrorType::TupleCons,
        |name, expression| {
            fields.push((
                name.clone(),
                eval(expression, exec, invoke, context)?.into_static(exec)?,
            ));
            Ok(())
        },
    )?;
    runtime_cost(ClarityCostFunction::TupleCons, exec, fields.len())?;
    if fields.iter().any(|(_, value)| value.retains_payload()) {
        let fields = fields
            .into_iter()
            .map(|(name, value)| value.into_shared(exec.epoch()).map(|value| (name, value)))
            .collect::<Result<Vec<_>, _>>()?;
        SharedPackedValue::tuple(fields, exec.epoch())
            .map(ValueRef::from_shared)
            .map_err(composite_vm_error)
    } else {
        let fields = fields
            .into_iter()
            .map(|(name, value)| value.into_owned().map(|value| (name, value)))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(ValueRef::Owned(Value::Tuple(TupleData::from_data(fields)?)))
    }
}

/// Append one child with a persistent shared prefix and a bounded owned tail.
pub fn append(
    args: &[SymbolicExpression],
    exec: &mut ExecutionState,
    invoke: &InvocationContext,
    context: &LocalContext,
) -> Result<ValueRef<'static>, VmExecutionError> {
    check_argument_count(2, args)?;
    let source = eval(&args[0], exec, invoke, context)?.into_static(exec)?;
    let TypeSignature::SequenceType(SequenceSubtype::ListType(schema)) = source.type_signature()?
    else {
        return Err(RuntimeCheckErrorKind::Unreachable("Expected list application".into()).into());
    };
    let (entry_type, size) = schema.destruct();
    let child = eval(&args[1], exec, invoke, context)?;
    let child_type = child.type_signature()?;
    runtime_cost(
        ClarityCostFunction::Append,
        exec,
        u64::from(cmp::max(entry_type.size()?, child_type.size()?)),
    )?;
    let child = child.into_static(exec)?;
    if entry_type.is_no_type() {
        assert_eq!(size, 0);
        return ValueRef::list_from(vec![child], exec.epoch());
    }
    let next_type = TypeSignature::least_supertype(exec.epoch(), &entry_type, &child_type)?;
    if !source.retains_payload() && !child.retains_payload() {
        let Value::Sequence(SequenceData::List(mut list)) = source.into_owned()? else {
            unreachable!("list type checked")
        };
        let (child, _) = Value::sanitize_value(exec.epoch(), &next_type, child.into_owned()?)
            .ok_or(RuntimeCheckErrorKind::ListTypesMustMatch)?;
        list.type_signature = ListTypeData::new_list(next_type, size + 1)?;
        list.data.push(child);
        return Ok(ValueRef::Owned(Value::Sequence(SequenceData::List(list))));
    }
    let child = child
        .into_shared(exec.epoch())?
        .sanitize(exec.epoch(), &next_type)
        .ok_or(RuntimeCheckErrorKind::ListTypesMustMatch)?
        .0;
    let schema = ListTypeData::new_list(next_type, size + 1)?;
    source
        .into_shared(exec.epoch())?
        .append_list(child, schema)
        .map(ValueRef::from_shared)
        .map_err(composite_vm_error)
}

/// Preserve retained tuple fields through a shallow merge.
pub fn tuple_merge<'a>(
    mut args: Vec<ValueRef<'a>>,
    exec: &mut ExecutionState,
    invoke: &InvocationContext,
) -> Result<ValueRef<'a>, VmExecutionError> {
    check_argument_count(2, &args)?;
    if !args
        .iter()
        .any(|value| matches!(value, ValueRef::Packed(_)))
    {
        return super::tuples::tuple_merge(
            args.into_iter()
                .map(ValueRef::into_owned)
                .collect::<Result<Vec<_>, _>>()?,
            exec,
            invoke,
        )
        .map(ValueRef::Owned);
    }
    let update = args.pop().expect("checked arity");
    let base = args.pop().expect("checked arity");
    for value in [&base, &update] {
        if !value.is_tuple()? {
            return Err(RuntimeCheckErrorKind::Unreachable(bounded_format!(
                "Expected tuple: {}",
                value.type_signature()?
            ))
            .into());
        }
    }
    base.into_shared(exec.epoch())?
        .merge_tuple(update.into_shared(exec.epoch())?, exec.epoch())
        .map(ValueRef::from_shared)
        .map_err(composite_vm_error)
}

/// Serialize the logical view directly into the required output buffer.
pub fn to_consensus<'a>(value: ValueRef<'a>) -> Result<ValueRef<'a>, VmExecutionError> {
    let bytes = value
        .serialize_to_vec()
        .map_err(|_| VmInternalError::Expect("FATAL: failed to serialize to vec".into()))?;
    let Ok(buffer) = Value::buff_from(bytes) else {
        return Ok(ValueRef::Owned(Value::none()));
    };
    Ok(ValueRef::Owned(
        Value::some(buffer).unwrap_or_else(|_| Value::none()),
    ))
}

/// Concatenate already-evaluated values, keeping the owned fast path for wholly owned inputs.
fn concat_values(
    left: ValueRef<'static>,
    right: ValueRef<'static>,
    epoch: &StacksEpochId,
) -> Result<ValueRef<'static>, VmExecutionError> {
    // Legacy byte concatenation permits transient oversized sequences. Keep its error timing.
    let limit = match left.type_signature()? {
        TypeSignature::SequenceType(
            SequenceSubtype::BufferType(_) | SequenceSubtype::StringType(StringSubtype::ASCII(_)),
        ) => Some(MAX_VALUE_SIZE as usize),
        TypeSignature::SequenceType(SequenceSubtype::StringType(StringSubtype::UTF8(_))) => {
            Some(MAX_UTF8_VALUE_SIZE as usize)
        }
        _ => None,
    };
    let oversized = limit.is_some_and(|limit| {
        left.sequence_len().ok().flatten().unwrap_or(0)
            + right.sequence_len().ok().flatten().unwrap_or(0)
            > limit
    });
    if oversized || (!left.retains_payload() && !right.retains_payload()) {
        let mut left = left.into_owned()?;
        let right = right.into_owned()?;
        if let (Value::Sequence(a), Value::Sequence(b)) = (&mut left, right) {
            a.concat(epoch, b)?;
            return Ok(ValueRef::Owned(left));
        }
        unreachable!("sequence operands checked before concatenation")
    }
    let left = left.into_shared(epoch)?;
    let right = right.into_shared(epoch)?;
    left.concat_sequence(right, epoch)
        .map(ValueRef::from_shared)
        .map_err(composite_vm_error)
}

/// Preserve epoch-specific evaluation and charging while retaining concatenated regions.
pub fn concat(
    args: &[SymbolicExpression],
    exec: &mut ExecutionState,
    invoke: &InvocationContext,
    context: &LocalContext,
) -> Result<ValueRef<'static>, VmExecutionError> {
    let variadic = *exec.epoch() >= StacksEpochId::Epoch40;
    let legacy_cost = *exec.epoch() == StacksEpochId::Epoch20;
    if variadic {
        check_arguments_at_least(2, args)?;
    } else {
        check_argument_count(2, args)?;
    }
    let mut values = Vec::with_capacity(args.len());
    let mut total_len = 0u64;
    for arg in args {
        let value = eval(arg, exec, invoke, context)?.into_static(exec)?;
        if variadic {
            let Some(len) = value.sequence_len()? else {
                runtime_cost(ClarityCostFunction::Concat, exec, 1)?;
                return Err(RuntimeCheckErrorKind::Unreachable(bounded_format!(
                    "Expected sequence: {}",
                    value.type_signature()?
                ))
                .into());
            };
            total_len = total_len.cost_overflow_add(len as u64)?;
        }
        values.push(value);
    }
    if legacy_cost {
        runtime_cost(
            ClarityCostFunction::Concat,
            exec,
            u64::from(values[0].size()?).cost_overflow_add(u64::from(values[1].size()?))?,
        )?;
        for value in &values {
            if value.sequence_len()?.is_none() {
                return Err(RuntimeCheckErrorKind::Unreachable(bounded_format!(
                    "Expected sequence: {}",
                    value.type_signature()?
                ))
                .into());
            }
        }
    } else if variadic {
        runtime_cost(ClarityCostFunction::Concat, exec, total_len)?;
    } else {
        match (values[0].sequence_len()?, values[1].sequence_len()?) {
            (Some(a), Some(b)) => runtime_cost(
                ClarityCostFunction::Concat,
                exec,
                (a as u64).cost_overflow_add(b as u64)?,
            )?,
            (Some(_), None) => {
                runtime_cost(ClarityCostFunction::Concat, exec, 1)?;
                return Err(RuntimeCheckErrorKind::TypeValueError(
                    Box::new(values[0].type_signature()?),
                    values[1].as_ref().to_error_string(),
                )
                .into());
            }
            _ => {
                runtime_cost(ClarityCostFunction::Concat, exec, 1)?;
                return Err(RuntimeCheckErrorKind::Unreachable(bounded_format!(
                    "Expected sequence: {}",
                    values[0].type_signature()?
                ))
                .into());
            }
        }
    }
    let mut values = values.into_iter();
    let mut result = values.next().expect("checked arity");
    for value in values {
        result = concat_values(result, value, exec.epoch())?;
    }
    Ok(result)
}

/// Replace a selected element while retaining the unaffected sequence regions.
pub fn replace_at(
    args: &[SymbolicExpression],
    exec: &mut ExecutionState,
    invoke: &InvocationContext,
    context: &LocalContext,
) -> Result<ValueRef<'static>, VmExecutionError> {
    check_argument_count(3, args)?;
    let source = eval(&args[0], exec, invoke, context)?;
    let source_type = source.type_signature()?;
    runtime_cost(ClarityCostFunction::ReplaceAt, exec, source_type.size()?)?;
    let TypeSignature::SequenceType(subtype) = &source_type else {
        return Err(RuntimeCheckErrorKind::Unreachable(bounded_format!(
            "Expected sequence: {source_type}"
        ))
        .into());
    };
    let expected_item = subtype.unit_type();
    let index = eval(&args[1], exec, invoke, context)?;
    let item = eval(&args[2], exec, invoke, context)?;
    if expected_item != TypeSignature::NoType
        && !expected_item.admits_type(exec.epoch(), &item.type_signature()?)?
    {
        return Err(RuntimeCheckErrorKind::TypeValueError(
            Box::new(expected_item),
            item.as_ref().to_error_string(),
        )
        .into());
    }
    let Some(index_value) = index.as_uint()? else {
        return Err(RuntimeCheckErrorKind::TypeValueError(
            Box::new(TypeSignature::UIntType),
            index.as_ref().to_error_string(),
        )
        .into());
    };
    let Ok(index) = usize::try_from(index_value) else {
        return Ok(ValueRef::Owned(Value::none()));
    };
    let source = source.into_static(exec)?;
    if index >= source.sequence_len()?.expect("sequence type") {
        return Ok(ValueRef::Owned(Value::none()));
    }
    let item = item.into_static(exec)?;
    if !source.retains_payload() && !item.retains_payload() {
        let Value::Sequence(source) = source.into_owned()? else {
            unreachable!("sequence type")
        };
        return source
            .replace_at(exec.epoch(), index, item.into_owned()?)
            .map(ValueRef::Owned)
            .map_err(Into::into);
    }
    let source = source.into_shared(exec.epoch())?;
    let item = item.into_shared(exec.epoch())?;
    let result = if matches!(subtype, SequenceSubtype::ListType(_)) {
        source.replace_list(index, item)
    } else {
        source.replace_bytes(index, item)
    }
    .map_err(composite_vm_error)?;
    ValueRef::from_shared(result).into_optional()
}
