//! Cost equivalence between borrowed natives and their original owned implementations.

use stacks_common::consts::CHAIN_ID_TESTNET;
use stacks_common::types::StacksEpochId;

use super::{lookup_reserved_functions, options};
use crate::vm::callables::{BuiltinKind, CallableType, NativeHandle};
use crate::vm::contexts::{ExecutionState, InvocationContext};
use crate::vm::costs::{CostTracker, ExecutionCost, LimitedCostTracker};
use crate::vm::database::MemoryBackingStore;
use crate::vm::errors::{RuntimeCheckErrorKind, VmExecutionError};
use crate::vm::types::codec::packed::{PackedValue, PackedValueVersion, SharedPackedValue};
use crate::vm::types::{QualifiedContractIdentifier, TypeSignature};
use crate::vm::{
    CallStack, ClarityName, ClarityVersion, ContractContext, GlobalContext, LocalContext,
    SymbolicExpression, Value, ValueCow, apply,
};

/// The original owned `begin` implementation, used as a reference dispatch target.
fn owned_begin(mut args: Vec<Value>) -> Result<Value, VmExecutionError> {
    args.pop().ok_or_else(|| {
        RuntimeCheckErrorKind::Unreachable("Requires at least args: 1 got 0".into()).into()
    })
}

/// Evaluate one builtin with literal, owned-binding, or packed-binding arguments.
fn evaluate(
    callable: &CallableType,
    values: &[Value],
    representation: usize,
    epoch: StacksEpochId,
) -> (Result<Value, String>, ExecutionCost, u64) {
    let mut local = LocalContext::new();
    let mut owners = Vec::new();
    let args: Vec<_> = values
        .iter()
        .enumerate()
        .map(|(index, value)| {
            if representation == 0 {
                return SymbolicExpression::atom_value(value.clone());
            }
            let name = ClarityName::try_from(format!("arg{index}")).unwrap();
            let binding = if representation == 1 {
                ValueCow::Owned(value.clone())
            } else {
                let schema = TypeSignature::type_of(value).unwrap();
                let bytes = PackedValue::encode(PackedValueVersion::V1, value).unwrap();
                let packed =
                    SharedPackedValue::copy_from(bytes.as_bytes(), &schema, &epoch).unwrap();
                owners.push(packed.clone());
                ValueCow::Packed(packed)
            };
            local.variables.insert(name.clone(), binding);
            SymbolicExpression::atom(name)
        })
        .collect();
    let mut store = MemoryBackingStore::new();
    let mut global = GlobalContext::new(
        false,
        CHAIN_ID_TESTNET,
        store.as_clarity_db(),
        LimitedCostTracker::new_with_limit(epoch, ExecutionCost::max_value()),
        epoch,
    );
    let mut stack = CallStack::new();
    let mut state = ExecutionState {
        global_context: &mut global,
        call_stack: &mut stack,
    };
    let contract = ContractContext::new(
        QualifiedContractIdentifier::transient(),
        ClarityVersion::Clarity1,
    );
    let invoke = InvocationContext {
        contract_context: &contract,
        sender: None,
        caller: None,
        sponsor: None,
    };
    // Consuming the result must not charge for the already-consumed input again.
    let result = apply(callable, &args, &mut state, &invoke, &local)
        .and_then(|value| value.into_static(&mut state));
    if matches!(
        callable,
        CallableType::Builtin {
            kind: BuiltinKind::BorrowingNative(..),
            ..
        }
    ) && result.is_ok()
    {
        assert!(owners.iter().all(|value| !value.is_materialized()));
    }
    let result = result
        .and_then(|value| value.into_owned())
        .map_err(|error| format!("{error:?}"));
    (
        result,
        global.cost_track.get_total(),
        global.cost_track.get_memory(),
    )
}

/// Native results have owned-result cost semantics even when backed by shared bytes.
#[test]
fn borrowed_native_result_costs_match_owned_dispatch() {
    let payload = Value::buff_from(vec![7; 2048]).unwrap();
    let fixtures = [
        (
            "begin",
            NativeHandle::MoreArg(&owned_begin),
            vec![payload.clone()],
        ),
        (
            "unwrap-panic",
            NativeHandle::SingleArg(&options::native_unwrap),
            vec![Value::some(payload.clone()).unwrap()],
        ),
        (
            "unwrap-panic",
            NativeHandle::SingleArg(&options::native_unwrap),
            vec![Value::okay(payload.clone()).unwrap()],
        ),
        (
            "unwrap-panic",
            NativeHandle::SingleArg(&options::native_unwrap),
            vec![Value::none()],
        ),
        (
            "unwrap-err-panic",
            NativeHandle::SingleArg(&options::native_unwrap_err),
            vec![Value::error(payload.clone()).unwrap()],
        ),
        (
            "unwrap!",
            NativeHandle::DoubleArg(&options::native_unwrap_or_ret),
            vec![Value::some(payload.clone()).unwrap(), Value::UInt(9)],
        ),
        (
            "unwrap!",
            NativeHandle::DoubleArg(&options::native_unwrap_or_ret),
            vec![Value::none(), Value::UInt(9)],
        ),
        (
            "unwrap-err!",
            NativeHandle::DoubleArg(&options::native_unwrap_err_or_ret),
            vec![Value::error(payload.clone()).unwrap(), Value::UInt(9)],
        ),
        (
            "default-to",
            NativeHandle::DoubleArg(&options::native_default_to),
            vec![payload.clone(), Value::none()],
        ),
        (
            "default-to",
            NativeHandle::DoubleArg(&options::native_default_to),
            vec![payload.clone(), Value::some(payload.clone()).unwrap()],
        ),
        (
            "try!",
            NativeHandle::SingleArg(&options::native_try_ret),
            vec![Value::some(payload.clone()).unwrap()],
        ),
        (
            "try!",
            NativeHandle::SingleArg(&options::native_try_ret),
            vec![Value::okay(payload.clone()).unwrap()],
        ),
        (
            "try!",
            NativeHandle::SingleArg(&options::native_try_ret),
            vec![Value::error(payload).unwrap()],
        ),
    ];
    let mut differences = Vec::new();
    for (name, native, values) in fixtures {
        let borrowed = lookup_reserved_functions(name, &ClarityVersion::Clarity1).unwrap();
        let CallableType::Builtin {
            kind: BuiltinKind::BorrowingNative(rust_name, _, cost),
            ..
        } = &borrowed
        else {
            panic!("expected borrowing native {name}");
        };
        let owned = CallableType::Builtin {
            clarity_name: name,
            kind: BuiltinKind::Native(rust_name, native, cost.clone()),
        };
        for epoch in [
            StacksEpochId::Epoch2_05,
            StacksEpochId::Epoch33,
            StacksEpochId::Epoch40,
        ] {
            for representation in 0..3 {
                let expected = evaluate(&owned, &values, representation, epoch);
                let actual = evaluate(&borrowed, &values, representation, epoch);
                if expected != actual {
                    differences.push(format!(
                        "{name} {epoch:?} representation={representation}: result_match={}, expected_cost={:?}, actual_cost={:?}, memory={:?}",
                        expected.0 == actual.0, expected.1, actual.1, (expected.2, actual.2),
                    ));
                }
            }
        }
    }
    assert!(differences.is_empty(), "{}", differences.join("\n"));
}
