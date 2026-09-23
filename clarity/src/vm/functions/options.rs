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

use stacks_common::bounded_format;

use crate::vm::Value::CallableContract;
use crate::vm::contexts::{ExecutionState, InvocationContext, LocalContext};
use crate::vm::costs::cost_functions::ClarityCostFunction;
use crate::vm::costs::{CostTracker, MemoryConsumer, runtime_cost};
use crate::vm::errors::{
    EarlyReturnError, RuntimeCheckErrorKind, RuntimeError, VmExecutionError, VmInternalError,
    check_arguments_at_least,
};
use crate::vm::types::{CallableData, OptionalData, ResponseData, TypeSignature, Value};
use crate::vm::{self, ClarityName, ClarityVersion, SymbolicExpression, ValueRef};

/// Unwrap an optional or committed response while retaining packed backing storage.
pub fn native_unwrap_ref<'value>(
    input: ValueRef<'value>,
) -> Result<ValueRef<'value>, VmExecutionError> {
    if input.is_optional()? {
        return input
            .optional_child()?
            .ok_or_else(|| RuntimeError::UnwrapFailure.into());
    }
    if input.is_response()? {
        let (committed, child) = input.response_child()?;
        return committed
            .then_some(child)
            .ok_or_else(|| RuntimeError::UnwrapFailure.into());
    }
    Err(RuntimeCheckErrorKind::Unreachable(bounded_format!(
        "Expected optional or response value: {}",
        input.as_ref()
    ))
    .into())
}

/// Unwrap an optional or committed response, returning `thrown` on failure.
pub fn native_unwrap_or_ret_ref<'value>(
    input: ValueRef<'value>,
    thrown: ValueRef<'value>,
) -> Result<ValueRef<'value>, VmExecutionError> {
    match native_unwrap_ref(input) {
        Ok(value) => Ok(value),
        Err(VmExecutionError::Runtime(RuntimeError::UnwrapFailure, _)) => {
            Err(EarlyReturnError::UnwrapFailed(Box::new(thrown.into_owned()?)).into())
        }
        Err(error) => Err(error),
    }
}

/// Unwrap the error branch of a response while retaining packed backing storage.
pub fn native_unwrap_err_ref<'value>(
    input: ValueRef<'value>,
) -> Result<ValueRef<'value>, VmExecutionError> {
    if !input.is_response()? {
        return Err(RuntimeCheckErrorKind::Unreachable(bounded_format!(
            "Expected response value: {}",
            input.as_ref()
        ))
        .into());
    }
    let (committed, child) = input.response_child()?;
    (!committed)
        .then_some(child)
        .ok_or_else(|| RuntimeError::UnwrapFailure.into())
}

/// Unwrap an error response, returning `thrown` on failure.
pub fn native_unwrap_err_or_ret_ref<'value>(
    input: ValueRef<'value>,
    thrown: ValueRef<'value>,
) -> Result<ValueRef<'value>, VmExecutionError> {
    match native_unwrap_err_ref(input) {
        Ok(value) => Ok(value),
        Err(VmExecutionError::Runtime(RuntimeError::UnwrapFailure, _)) => {
            Err(EarlyReturnError::UnwrapFailed(Box::new(thrown.into_owned()?)).into())
        }
        Err(error) => Err(error),
    }
}

/// Implement `try!` without materializing its success branch.
pub fn native_try_ret_ref<'value>(
    input: ValueRef<'value>,
) -> Result<ValueRef<'value>, VmExecutionError> {
    if input.is_optional()? {
        return input
            .optional_child()?
            .ok_or_else(|| EarlyReturnError::UnwrapFailed(Box::new(Value::none())).into());
    }
    if input.is_response()? {
        let (committed, child) = input.response_child()?;
        return if committed {
            Ok(child)
        } else {
            let value = Value::error(child.into_owned()?).map_err(|_| {
                VmInternalError::Expect(
                    "BUG: Failed to construct new response type from old response type".into(),
                )
            })?;
            Err(EarlyReturnError::UnwrapFailed(Box::new(value)).into())
        };
    }
    Err(RuntimeCheckErrorKind::Unreachable(bounded_format!(
        "Expected optional or response value: {}",
        input.as_ref()
    ))
    .into())
}

/// Test whether an optional has an active child without materializing it.
pub fn native_is_some_ref(input: ValueRef<'_>) -> Result<ValueRef<'_>, VmExecutionError> {
    if !input.is_optional()? {
        return Err(RuntimeCheckErrorKind::Unreachable(bounded_format!(
            "Expected option value: {}",
            input.as_ref()
        ))
        .into());
    }
    Ok(ValueRef::Owned(Value::Bool(
        input.optional_child()?.is_some(),
    )))
}

/// Test whether an optional is empty without materializing it.
pub fn native_is_none_ref(input: ValueRef<'_>) -> Result<ValueRef<'_>, VmExecutionError> {
    native_is_some_ref(input).and_then(|value| match value {
        ValueRef::Owned(Value::Bool(is_some)) => Ok(ValueRef::Owned(Value::Bool(!is_some))),
        _ => Err(VmInternalError::Expect("is-some must return a Boolean".into()).into()),
    })
}

/// Test whether a response is committed without materializing it.
pub fn native_is_okay_ref(input: ValueRef<'_>) -> Result<ValueRef<'_>, VmExecutionError> {
    if !input.is_response()? {
        return Err(RuntimeCheckErrorKind::Unreachable(bounded_format!(
            "Expected response value: {}",
            input.as_ref()
        ))
        .into());
    }
    let (committed, _) = input.response_child()?;
    Ok(ValueRef::Owned(Value::Bool(committed)))
}

/// Test whether a response is an error without materializing it.
pub fn native_is_err_ref(input: ValueRef<'_>) -> Result<ValueRef<'_>, VmExecutionError> {
    native_is_okay_ref(input).and_then(|value| match value {
        ValueRef::Owned(Value::Bool(is_okay)) => Ok(ValueRef::Owned(Value::Bool(!is_okay))),
        _ => Err(VmInternalError::Expect("is-ok must return a Boolean".into()).into()),
    })
}

/// Return an optional child or the caller-provided default without materializing either branch.
pub fn native_default_to_ref<'value>(
    default: ValueRef<'value>,
    input: ValueRef<'value>,
) -> Result<ValueRef<'value>, VmExecutionError> {
    if !input.is_optional()? {
        return Err(RuntimeCheckErrorKind::Unreachable(bounded_format!(
            "Expected option value: {}",
            input.as_ref()
        ))
        .into());
    }
    Ok(input.optional_child()?.unwrap_or(default))
}

fn inner_unwrap(to_unwrap: Value) -> Result<Option<Value>, VmExecutionError> {
    let result = match to_unwrap {
        Value::Optional(data) => data.data.map(|data| *data),
        Value::Response(data) => {
            if data.committed {
                Some(*data.data)
            } else {
                None
            }
        }
        _ => {
            return Err(RuntimeCheckErrorKind::Unreachable(bounded_format!(
                "Expected optional or response value: {to_unwrap}"
            ))
            .into());
        }
    };

    Ok(result)
}

fn inner_unwrap_err(to_unwrap: Value) -> Result<Option<Value>, VmExecutionError> {
    let result = match to_unwrap {
        Value::Response(data) => {
            if !data.committed {
                Some(*data.data)
            } else {
                None
            }
        }
        _ => {
            return Err(RuntimeCheckErrorKind::Unreachable(bounded_format!(
                "Expected response value: {to_unwrap}"
            ))
            .into());
        }
    };

    Ok(result)
}

pub fn native_unwrap(input: Value) -> Result<Value, VmExecutionError> {
    inner_unwrap(input).and_then(|opt_value| match opt_value {
        Some(v) => Ok(v),
        None => Err(RuntimeError::UnwrapFailure.into()),
    })
}

pub fn native_unwrap_or_ret(input: Value, thrown: Value) -> Result<Value, VmExecutionError> {
    inner_unwrap(input).and_then(|opt_value| match opt_value {
        Some(v) => Ok(v),
        None => Err(EarlyReturnError::UnwrapFailed(Box::new(thrown)).into()),
    })
}

pub fn native_unwrap_err(input: Value) -> Result<Value, VmExecutionError> {
    inner_unwrap_err(input).and_then(|opt_value| match opt_value {
        Some(v) => Ok(v),
        None => Err(RuntimeError::UnwrapFailure.into()),
    })
}

pub fn native_unwrap_err_or_ret(input: Value, thrown: Value) -> Result<Value, VmExecutionError> {
    inner_unwrap_err(input).and_then(|opt_value| match opt_value {
        Some(v) => Ok(v),
        None => Err(EarlyReturnError::UnwrapFailed(Box::new(thrown)).into()),
    })
}

pub fn native_try_ret(input: Value) -> Result<Value, VmExecutionError> {
    match input {
        Value::Optional(data) => match data.data {
            Some(data) => Ok(*data),
            None => Err(EarlyReturnError::UnwrapFailed(Box::new(Value::none())).into()),
        },
        Value::Response(data) => {
            if data.committed {
                Ok(*data.data)
            } else {
                let short_return_val = Value::error(*data.data).map_err(|_| {
                    VmInternalError::Expect(
                        "BUG: Failed to construct new response type from old response type".into(),
                    )
                })?;
                Err(EarlyReturnError::UnwrapFailed(Box::new(short_return_val)).into())
            }
        }
        _ => Err(RuntimeCheckErrorKind::Unreachable(bounded_format!(
            "Expected optional or response value: {input}"
        ))
        .into()),
    }
}

fn eval_with_new_binding(
    body: &SymbolicExpression,
    bind_name: ClarityName,
    bind_value: Value,
    exec_state: &mut ExecutionState,
    invoke_ctx: &InvocationContext,
    context: &LocalContext,
) -> Result<Value, VmExecutionError> {
    let mut inner_context = context.extend()?;
    if vm::is_reserved(
        &bind_name,
        invoke_ctx.contract_context.get_clarity_version(),
    ) || invoke_ctx
        .contract_context
        .lookup_function(&bind_name)
        .is_some()
        || inner_context.lookup_variable(&bind_name).is_some()
    {
        return Err(RuntimeCheckErrorKind::NameAlreadyUsed(bind_name.into()).into());
    }

    let memory_use = bind_value.get_memory_use()?;
    exec_state.add_memory(memory_use)?;

    if *invoke_ctx.contract_context.get_clarity_version() >= ClarityVersion::Clarity2
        && let CallableContract(trait_data) = &bind_value
    {
        inner_context.callable_contracts.insert(
            bind_name.clone(),
            CallableData {
                contract_identifier: trait_data.contract_identifier.clone(),
                trait_identifier: trait_data.trait_identifier.clone(),
            },
        );
    }
    inner_context.variables.insert(bind_name, bind_value.into());
    let result = vm::eval(body, exec_state, invoke_ctx, &inner_context)
        .and_then(|v| v.clone_with_cost(exec_state));

    exec_state.drop_memory(memory_use)?;

    result
}

fn special_match_opt(
    input: OptionalData,
    args: &[SymbolicExpression],
    exec_state: &mut ExecutionState,
    invoke_ctx: &InvocationContext,
    context: &LocalContext,
) -> Result<Value, VmExecutionError> {
    if args.len() != 3 {
        Err(RuntimeCheckErrorKind::Unreachable(bounded_format!(
            "Bad match option syntax: args {} != 3",
            args.len()
        )))?;
    }

    let bind_name = args[0]
        .match_atom()
        .ok_or_else(|| {
            RuntimeCheckErrorKind::Unreachable("Bad match option syntax: expected name".into())
        })?
        .clone();
    let some_branch = &args[1];
    let none_branch = &args[2];

    match input.data {
        Some(data) => eval_with_new_binding(
            some_branch,
            bind_name,
            *data,
            exec_state,
            invoke_ctx,
            context,
        ),
        None => vm::eval(none_branch, exec_state, invoke_ctx, context)
            .and_then(|v| v.clone_with_cost(exec_state)),
    }
}

fn special_match_resp(
    input: ResponseData,
    args: &[SymbolicExpression],
    exec_state: &mut ExecutionState,
    invoke_ctx: &InvocationContext,
    context: &LocalContext,
) -> Result<Value, VmExecutionError> {
    if args.len() != 4 {
        Err(RuntimeCheckErrorKind::Unreachable(bounded_format!(
            "Bad match response syntax: args {} != 4",
            args.len()
        )))?;
    }

    let ok_bind_name = args[0]
        .match_atom()
        .ok_or_else(|| {
            RuntimeCheckErrorKind::Unreachable("Bad match response syntax: expected name".into())
        })?
        .clone();
    let ok_branch = &args[1];
    let err_bind_name = args[2]
        .match_atom()
        .ok_or_else(|| {
            RuntimeCheckErrorKind::Unreachable("Bad match response syntax: expected name".into())
        })?
        .clone();
    let err_branch = &args[3];

    if input.committed {
        eval_with_new_binding(
            ok_branch,
            ok_bind_name,
            *input.data,
            exec_state,
            invoke_ctx,
            context,
        )
    } else {
        eval_with_new_binding(
            err_branch,
            err_bind_name,
            *input.data,
            exec_state,
            invoke_ctx,
            context,
        )
    }
}

pub fn special_match(
    args: &[SymbolicExpression],
    exec_state: &mut ExecutionState,
    invoke_ctx: &InvocationContext,
    context: &LocalContext,
) -> Result<Value, VmExecutionError> {
    check_arguments_at_least(1, args)?;

    let input =
        { vm::eval(&args[0], exec_state, invoke_ctx, context)?.clone_with_cost(exec_state)? };

    runtime_cost(ClarityCostFunction::Match, exec_state, 0)?;

    match input {
        Value::Response(data) => {
            special_match_resp(data, &args[1..], exec_state, invoke_ctx, context)
        }
        Value::Optional(data) => {
            special_match_opt(data, &args[1..], exec_state, invoke_ctx, context)
        }
        _ => Err(RuntimeCheckErrorKind::Unreachable(bounded_format!(
            "Bad match input: {}",
            TypeSignature::type_of(&input)?
        ))
        .into()),
    }
}

/// Bind one reference-backed match value and stabilize the selected branch result.
fn eval_with_new_binding_ref(
    body: &SymbolicExpression,
    bind_name: ClarityName,
    bind_value: ValueRef<'_>,
    exec_state: &mut ExecutionState,
    invoke_ctx: &InvocationContext,
    context: &LocalContext,
) -> Result<ValueRef<'static>, VmExecutionError> {
    let mut inner_context = context.extend()?;
    if vm::is_reserved(
        &bind_name,
        invoke_ctx.contract_context.get_clarity_version(),
    ) || invoke_ctx
        .contract_context
        .lookup_function(&bind_name)
        .is_some()
        || inner_context.lookup_variable(&bind_name).is_some()
    {
        return Err(RuntimeCheckErrorKind::NameAlreadyUsed(bind_name.into()).into());
    }

    let memory_use = bind_value.get_memory_use()?;
    exec_state.add_memory(memory_use)?;
    let bind_value = bind_value.into_cow();
    if *invoke_ctx.contract_context.get_clarity_version() >= ClarityVersion::Clarity2
        && bind_value.as_value_ref().is_callable()?
        && let CallableContract(trait_data) = bind_value.as_value()
    {
        inner_context.callable_contracts.insert(
            bind_name.clone(),
            CallableData {
                contract_identifier: trait_data.contract_identifier.clone(),
                trait_identifier: trait_data.trait_identifier.clone(),
            },
        );
    }
    inner_context.variables.insert(bind_name, bind_value);
    let result = vm::eval(body, exec_state, invoke_ctx, &inner_context)
        .and_then(|value| value.into_static(exec_state));
    exec_state.drop_memory(memory_use)?;
    result
}

/// Evaluate `match` while retaining packed optional/response children in branch bindings.
pub fn special_match_ref(
    args: &[SymbolicExpression],
    exec_state: &mut ExecutionState,
    invoke_ctx: &InvocationContext,
    context: &LocalContext,
) -> Result<ValueRef<'static>, VmExecutionError> {
    check_arguments_at_least(1, args)?;
    let input = vm::eval(&args[0], exec_state, invoke_ctx, context)?;
    input.charge_clone_cost(exec_state)?;
    runtime_cost(ClarityCostFunction::Match, exec_state, 0)?;

    if input.is_optional()? {
        if args.len() != 4 {
            return Err(RuntimeCheckErrorKind::Unreachable(bounded_format!(
                "Bad match option syntax: args {} != 3",
                args.len() - 1
            ))
            .into());
        }
        let bind_name = args[1]
            .match_atom()
            .ok_or_else(|| {
                RuntimeCheckErrorKind::Unreachable("Bad match option syntax: expected name".into())
            })?
            .clone();
        return match input.optional_child()? {
            Some(child) => eval_with_new_binding_ref(
                &args[2], bind_name, child, exec_state, invoke_ctx, context,
            ),
            None => vm::eval(&args[3], exec_state, invoke_ctx, context)?.into_static(exec_state),
        };
    }

    if input.is_response()? {
        if args.len() != 5 {
            return Err(RuntimeCheckErrorKind::Unreachable(bounded_format!(
                "Bad match response syntax: args {} != 4",
                args.len() - 1
            ))
            .into());
        }
        let (committed, child) = input.response_child()?;
        let (name_index, body_index) = if committed { (1, 2) } else { (3, 4) };
        let bind_name = args[name_index]
            .match_atom()
            .ok_or_else(|| {
                RuntimeCheckErrorKind::Unreachable(
                    "Bad match response syntax: expected name".into(),
                )
            })?
            .clone();
        return eval_with_new_binding_ref(
            &args[body_index],
            bind_name,
            child,
            exec_state,
            invoke_ctx,
            context,
        );
    }

    Err(RuntimeCheckErrorKind::Unreachable(bounded_format!(
        "Bad match input: {}",
        input.type_signature()?
    ))
    .into())
}

/// Construct an optional while retaining a packed child's original byte owner.
pub fn native_some_ref<'a>(input: ValueRef<'a>) -> Result<ValueRef<'a>, VmExecutionError> {
    input.into_optional()
}

pub fn native_some(input: Value) -> Result<Value, VmExecutionError> {
    Ok(Value::some(input)?)
}

fn is_some(input: Value) -> Result<bool, RuntimeCheckErrorKind> {
    match input {
        Value::Optional(ref data) => Ok(data.data.is_some()),
        _ => Err(RuntimeCheckErrorKind::Unreachable(bounded_format!(
            "Expected option value: {input}"
        ))),
    }
}

fn is_okay(input: Value) -> Result<bool, RuntimeCheckErrorKind> {
    match input {
        Value::Response(data) => Ok(data.committed),
        _ => Err(RuntimeCheckErrorKind::Unreachable(bounded_format!(
            "Expected response value: {input}"
        ))),
    }
}

pub fn native_is_some(input: Value) -> Result<Value, VmExecutionError> {
    Ok(is_some(input).map(Value::Bool)?)
}

pub fn native_is_none(input: Value) -> Result<Value, VmExecutionError> {
    Ok(is_some(input).map(|is_some| Value::Bool(!is_some))?)
}

pub fn native_is_okay(input: Value) -> Result<Value, VmExecutionError> {
    Ok(is_okay(input).map(Value::Bool)?)
}

pub fn native_is_err(input: Value) -> Result<Value, VmExecutionError> {
    Ok(is_okay(input).map(|is_ok| Value::Bool(!is_ok))?)
}

pub fn native_okay(input: Value) -> Result<Value, VmExecutionError> {
    Ok(Value::okay(input)?)
}

pub fn native_error(input: Value) -> Result<Value, VmExecutionError> {
    Ok(Value::error(input)?)
}

pub fn native_default_to(default: Value, input: Value) -> Result<Value, VmExecutionError> {
    match input {
        Value::Optional(data) => match data.data {
            Some(data) => Ok(*data),
            None => Ok(default),
        },
        _ => Err(RuntimeCheckErrorKind::Unreachable(bounded_format!(
            "Expected option value: {input}"
        ))
        .into()),
    }
}

/// Construct a response retaining a shared success payload.
pub fn native_okay_ref<'a>(input: ValueRef<'a>) -> Result<ValueRef<'a>, VmExecutionError> {
    input.into_response(true)
}

/// Construct a response retaining a shared error payload.
pub fn native_error_ref<'a>(input: ValueRef<'a>) -> Result<ValueRef<'a>, VmExecutionError> {
    input.into_response(false)
}
