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

use crate::vm::contexts::{ExecutionState, InvocationContext, LocalContext};
use crate::vm::costs::cost_functions::ClarityCostFunction;
use crate::vm::costs::runtime_cost;
use crate::vm::errors::{RuntimeCheckErrorKind, VmExecutionError, check_arguments_at_least};
use crate::vm::representations::SymbolicExpression;
use crate::vm::types::{TypeSignature, Value};
use crate::vm::{ValueRef, eval};

fn type_force_bool(value: &Value) -> Result<bool, RuntimeCheckErrorKind> {
    match *value {
        Value::Bool(boolean) => Ok(boolean),
        _ => Err(RuntimeCheckErrorKind::TypeValueError(
            Box::new(TypeSignature::BoolType),
            value.to_error_string(),
        )),
    }
}

/// Read a Boolean from an owned or packed value without materializing packed storage.
fn type_force_bool_ref(value: &ValueRef<'_>) -> Result<bool, VmExecutionError> {
    value.as_bool()?.ok_or_else(|| {
        RuntimeCheckErrorKind::TypeValueError(
            Box::new(TypeSignature::BoolType),
            value.as_ref().to_error_string(),
        )
        .into()
    })
}

pub fn special_or(
    args: &[SymbolicExpression],
    exec_state: &mut ExecutionState,
    invoke_ctx: &InvocationContext,
    context: &LocalContext,
) -> Result<Value, VmExecutionError> {
    check_arguments_at_least(1, args)?;

    runtime_cost(ClarityCostFunction::Or, exec_state, args.len())?;

    for arg in args.iter() {
        let evaluated = eval(arg, exec_state, invoke_ctx, context)?;
        let result = type_force_bool_ref(&evaluated)?;
        if result {
            return Ok(Value::Bool(true));
        }
    }

    Ok(Value::Bool(false))
}

pub fn special_and(
    args: &[SymbolicExpression],
    exec_state: &mut ExecutionState,
    invoke_ctx: &InvocationContext,
    context: &LocalContext,
) -> Result<Value, VmExecutionError> {
    check_arguments_at_least(1, args)?;

    runtime_cost(ClarityCostFunction::And, exec_state, args.len())?;

    for arg in args.iter() {
        let evaluated = eval(arg, exec_state, invoke_ctx, context)?;
        let result = type_force_bool_ref(&evaluated)?;
        if !result {
            return Ok(Value::Bool(false));
        }
    }

    Ok(Value::Bool(true))
}

pub fn native_not(input: Value) -> Result<Value, VmExecutionError> {
    let value = type_force_bool(&input)?;
    Ok(Value::Bool(!value))
}

/// Negate a Boolean without materializing packed storage.
pub fn native_not_ref(input: ValueRef<'_>) -> Result<ValueRef<'_>, VmExecutionError> {
    Ok(ValueRef::Owned(Value::Bool(!type_force_bool_ref(&input)?)))
}
