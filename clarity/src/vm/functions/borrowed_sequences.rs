//! Ownership, result and cost equivalence for packed sequence consumers.

use stacks_common::consts::CHAIN_ID_TESTNET;
use stacks_common::types::StacksEpochId;

use crate::vm::ast;
use crate::vm::callables::{DefineType, DefinedFunction};
use crate::vm::contexts::{ExecutionState, InvocationContext};
use crate::vm::costs::{ExecutionCost, LimitedCostTracker};
use crate::vm::database::MemoryBackingStore;
use crate::vm::types::codec::packed::{PackedValue, PackedValueVersion, SharedPackedValue};
use crate::vm::types::{QualifiedContractIdentifier, TupleData, TypeSignature};
use crate::vm::{
    CallStack, ClarityName, ClarityVersion, ContractContext, GlobalContext, LocalContext, Value,
    ValueCow, eval,
};

/// Encode one owned fixture while retaining its declared schema.
fn packed(value: &Value, epoch: StacksEpochId) -> SharedPackedValue {
    let record = PackedValue::encode(PackedValueVersion::V1, value).unwrap();
    SharedPackedValue::copy_from(
        record.as_bytes(),
        &TypeSignature::type_of(value).unwrap(),
        &epoch,
    )
    .unwrap()
}

/// Execute a real VM expression with identical owned or packed local bindings.
fn run(
    source: &str,
    borrowed: bool,
    epoch: StacksEpochId,
    version: ClarityVersion,
) -> (Result<Value, String>, ExecutionCost, u64, u64) {
    let payload = Value::buff_from(vec![0xab; 4096]).unwrap();
    let tuple = |flag| {
        Value::Tuple(
            TupleData::from_data(vec![
                (ClarityName::from_literal("flag"), Value::Bool(flag)),
                (ClarityName::from_literal("payload"), payload.clone()),
            ])
            .unwrap(),
        )
    };
    let first = tuple(true);
    let second = tuple(false);
    let items = Value::cons_list(vec![first.clone(), second.clone()], &epoch).unwrap();
    let buffers = Value::cons_list(vec![payload.clone(), payload.clone()], &epoch).unwrap();
    let nested = Value::cons_list(vec![buffers.clone(), buffers.clone()], &epoch).unwrap();
    let tuple_type = TypeSignature::type_of(&first).unwrap();
    let buffer_type = TypeSignature::type_of(&payload).unwrap();
    let list_type = TypeSignature::type_of(&buffers).unwrap();
    let mut locals = LocalContext::new();
    for (name, value) in [
        ("items", items),
        ("needle", second),
        ("initial", first),
        ("buffers", buffers),
        ("nested", nested),
        ("bytes", payload),
        (
            "text",
            Value::string_utf8_from_bytes("aé😀z".as_bytes().to_vec()).unwrap(),
        ),
        (
            "ascii",
            Value::string_ascii_from_bytes(b"abcdef".to_vec()).unwrap(),
        ),
    ] {
        let value = if borrowed {
            ValueCow::Packed(packed(&value, epoch))
        } else {
            ValueCow::Owned(value)
        };
        locals
            .variables
            .insert(ClarityName::from_literal(name), value);
    }
    let id = QualifiedContractIdentifier::transient();
    let parse = |source: &str| ast::parse(&id, source, version, epoch).unwrap().remove(0);
    let mut contract = ContractContext::new(id.clone(), version);
    for (name, arguments, body) in [
        (
            "flag",
            vec![("item", tuple_type.clone())],
            "(get flag item)",
        ),
        ("reject", vec![("item", tuple_type.clone())], "false"),
        (
            "count-flags",
            vec![
                ("item", tuple_type.clone()),
                ("acc", TypeSignature::UIntType),
            ],
            "(+ acc (if (get flag item) u1 u0))",
        ),
        (
            "pick",
            vec![("item", tuple_type.clone()), ("acc", tuple_type.clone())],
            "item",
        ),
        (
            "sum-buffer",
            vec![("item", buffer_type), ("acc", TypeSignature::UIntType)],
            "(+ acc (len item))",
        ),
        (
            "sum-list",
            vec![("item", list_type), ("acc", TypeSignature::UIntType)],
            "(fold sum-buffer item acc)",
        ),
        ("fail", vec![("item", tuple_type)], "(/ u1 u0)"),
    ] {
        let arguments = arguments
            .into_iter()
            .map(|(name, ty)| (ClarityName::from_literal(name), ty))
            .collect();
        contract.functions.insert(
            ClarityName::from_literal(name),
            DefinedFunction::new(
                arguments,
                parse(body),
                DefineType::Private,
                &ClarityName::from_literal(name),
                "",
            ),
        );
    }
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
    let invoke = InvocationContext {
        contract_context: &contract,
        sender: None,
        caller: None,
        sponsor: None,
    };
    SharedPackedValue::reset_materialization_count();
    let result = eval(&parse(source), &mut state, &invoke, &locals)
        .and_then(|value| value.into_static(&mut state))
        .and_then(|value| value.into_owned())
        .map_err(|error| format!("{error:?}"));
    (
        result,
        global.cost_track.get_total(),
        global.cost_track.get_memory(),
        SharedPackedValue::materialization_count(),
    )
}

/// Compound callback arguments, accumulators and element-at optionals retain their owners.
#[test]
fn compound_callbacks_and_projections_avoid_materialization() {
    for (epoch, version) in [
        (StacksEpochId::Epoch21, ClarityVersion::Clarity2),
        (StacksEpochId::Epoch40, ClarityVersion::Clarity6),
    ] {
        for expression in [
            "(fold count-flags items u0)",
            "(len (append buffers bytes))",
            "(len (concat buffers buffers))",
            "(len (concat bytes bytes))",
            "(len (concat text text))",
            "(len (unwrap-panic (slice? (concat text text) u2 u6)))",
            "(len (unwrap-panic (element-at? (concat buffers buffers) u2)))",
            "(get flag (unwrap-panic (element-at? (unwrap-panic (replace-at? items u0 initial)) u0)))",
            "(len (unwrap-panic (replace-at? bytes u0 0xaa)))",
            "(len (unwrap-panic (replace-at? text u1 u\"x\")))",
            "(len (unwrap-panic (as-max-len? buffers u3)))",
            "(len (get payload (merge initial {flag: false})))",
            "(len (get held {held: bytes}))",
            "(len (unwrap-panic (ok bytes)))",
            "(len (unwrap-err-panic (err bytes)))",
            "(len (unwrap-panic (to-consensus-buff? items)))",
            "(len (unwrap-panic (to-consensus-buff? (concat text text))))",
            "(len (map some buffers))",
            "(len (map some nested))",
            "(map flag items)",
            "(len (filter reject items))",
            "(get flag (fold pick items initial))",
            "(fold sum-list nested u0)",
            "(len (filter flag items))",
            "(fold count-flags (filter flag items) u0)",
            "(map flag (filter flag (filter flag items)))",
            "(get flag (unwrap-panic (element-at? (filter flag items) u0)))",
            "(is-eq (filter flag items) (filter flag items))",
            "(index-of? (filter flag items) initial)",
            "(fold count-flags (unwrap-panic (slice? items u0 u1)) u0)",
            "(map flag (unwrap-panic (slice? (filter flag items) u0 u1)))",
            "(len (filter flag (unwrap-panic (slice? items u0 u1))))",
            "(len (unwrap-panic (slice? buffers u0 u1)))",
            "(len (unwrap-panic (slice? nested u0 u1)))",
            "(len (unwrap-panic (slice? ascii u1 u3)))",
            "(map len buffers)",
            "(map > buffers buffers)",
            "(get flag (unwrap-panic (element-at? items u1)))",
            "(len (unwrap-panic (get payload (element-at? items u1))))",
            "(len (unwrap-panic (element-at? nested u0)))",
            "(len (unwrap-panic (element-at? buffers u0)))",
            "(is-eq (element-at? items u0) (some initial))",
            "(get flag (unwrap-panic (some initial)))",
            "(index-of? items needle)",
            "(index-of? buffers bytes)",
            "(map len text)",
            "(index-of? text u\"\\u{1f600}\")",
            "(> bytes 0xab)",
            "(< text u\"z\")",
            "(>= ascii \"abcdef\")",
            "(<= bytes bytes)",
            "(is-eq bytes bytes)",
            "(is-eq items items)",
            "(len (unwrap-panic (slice? bytes u10 u20)))",
            "(len (unwrap-panic (slice? text u1 u3)))",
            "(as-max-len? items u0)",
            "(len (unwrap-panic (to-ascii? bytes)))",
            "(to-ascii? text)",
        ] {
            // to-ascii? is unavailable before Clarity 4.
            if expression.contains("to-ascii?") && version < ClarityVersion::Clarity4 {
                continue;
            }
            let owned = run(expression, false, epoch, version);
            let borrowed = run(expression, true, epoch, version);
            assert!(owned.0.is_ok(), "{expression}: {:?}", owned.0);
            assert_eq!(
                (&owned.0, owned.1, owned.2),
                (&borrowed.0, borrowed.1, borrowed.2),
                "{expression}"
            );
            assert_eq!(borrowed.3, 0, "{expression} materialized a packed value");
        }
    }
}

/// Callback failures and rejected range operations preserve execution costs and cleanup.
#[test]
fn borrowed_sequence_errors_and_owned_outputs_match() {
    for expression in [
        "(map fail items)",
        "(slice? bytes u99999 u999999)",
        "(slice? text u3 u1)",
        "(element-at? items u99)",
        "(filter flag items)",
        "(slice? items u0 u1)",
        "(some items)",
        "(element-at? items u0)",
    ] {
        let owned = run(
            expression,
            false,
            StacksEpochId::Epoch40,
            ClarityVersion::Clarity6,
        );
        let borrowed = run(
            expression,
            true,
            StacksEpochId::Epoch40,
            ClarityVersion::Clarity6,
        );
        assert_eq!(
            (&owned.0, owned.1, owned.2),
            (&borrowed.0, borrowed.1, borrowed.2),
            "{expression}"
        );
    }
}
