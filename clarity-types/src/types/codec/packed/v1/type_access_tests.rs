//! Differential coverage for admitted accessors and their owned equivalents.
use super::super::{PackedValue, PackedValueVersion, SharedPackedValue};
use crate::types::signatures::CallableSubtype;
use crate::types::{
    CallableData, PrincipalData, QualifiedContractIdentifier, TraitIdentifier, TupleData,
    TupleTypeSignature, TypeSignature, Value,
};
use stacks_common::types::StacksEpochId;

/// Encode and strictly admit a fixture before exercising cheap accessors.
fn packed(value: &Value, expected: &TypeSignature) -> SharedPackedValue {
    let bytes = PackedValue::encode(PackedValueVersion::V1, value).unwrap();
    SharedPackedValue::copy_from(bytes.as_bytes(), expected, &StacksEpochId::latest()).unwrap()
}

/// Every runtime family retains owned equality, costs, lengths, and logical types.
#[test]
fn all_runtime_families_match_owned_access() {
    let contract = QualifiedContractIdentifier::transient();
    let mut values = vec![
        Value::Int(i128::MIN),
        Value::Int(-129),
        Value::Int(-128),
        Value::Int(0),
        Value::Int(i128::MAX),
        Value::UInt(0),
        Value::UInt(256),
        Value::UInt(u128::MAX),
        Value::Bool(false),
        Value::Bool(true),
        Value::buff_from(vec![0, 255, 1]).unwrap(),
        Value::string_ascii_from_bytes(b"ascii".to_vec()).unwrap(),
        Value::string_utf8_from_bytes("aé中🦀".as_bytes().to_vec()).unwrap(),
        Value::Principal(PrincipalData::Standard(contract.issuer.clone())),
        Value::Principal(PrincipalData::Contract(contract.clone())),
        Value::CallableContract(CallableData {
            contract_identifier: contract,
            trait_identifier: None,
        }),
        Value::none(),
        Value::some(Value::UInt(7)).unwrap(),
        Value::okay(Value::Bool(true)).unwrap(),
        Value::error(Value::Int(-1)).unwrap(),
        Value::list_from(vec![Value::UInt(0), Value::UInt(u128::MAX)]).unwrap(),
        Value::list_from(vec![Value::Int(0), Value::Int(i128::MIN)]).unwrap(),
        Value::list_from(vec![Value::Bool(true); 17]).unwrap(),
    ];
    let fixed = Value::from(
        TupleData::from_data(vec![
            ("a".try_into().unwrap(), Value::Bool(true)),
            ("b".try_into().unwrap(), Value::Bool(false)),
        ])
        .unwrap(),
    );
    values.push(fixed.clone());
    values.push(Value::list_from(vec![fixed.clone(); 4]).unwrap());
    values.push(Value::some(Value::okay(fixed).unwrap()).unwrap());
    values.push(Value::from(
        TupleData::from_data(vec![(
            "items".try_into().unwrap(),
            Value::list_from(vec![Value::some(Value::UInt(1)).unwrap(), Value::none()]).unwrap(),
        )])
        .unwrap(),
    ));
    for value in &values {
        assert_eq!(
            PackedValue::encoded_byte_len(PackedValueVersion::V1, value).unwrap(),
            PackedValue::encode(PackedValueVersion::V1, value)
                .unwrap()
                .as_bytes()
                .len()
        );
        let expected = TypeSignature::type_of(value).unwrap();
        let shared = packed(value, &expected);
        let body_len = PackedValue::encode(PackedValueVersion::V1, value)
            .unwrap()
            .as_bytes()
            .len()
            - PackedValueVersion::V1.header_len();
        for cap in [0, 1, 8, 25, 31, 256] {
            for budget in [0, 1, 2, 64] {
                let mut visits = budget;
                let lower = shared
                    .as_view()
                    .body_len_lower_bound(cap, &mut visits)
                    .unwrap();
                assert!(
                    lower <= body_len.min(cap),
                    "{value:?}: {lower} > {body_len}"
                );
                assert!(visits <= budget);
            }
        }
        let owned = shared.to_owned_value().unwrap();
        assert_eq!(
            shared.consensus_byte_len() as usize,
            value.serialize_to_vec().unwrap().len()
        );
        assert_eq!(shared.logical_size().unwrap(), owned.size().unwrap());
        assert_eq!(
            shared.logical_type().unwrap(),
            TypeSignature::type_of(&owned).unwrap()
        );
        for other in &values {
            assert_eq!(
                shared.as_view().value_eq_owned(other).unwrap(),
                owned == *other,
                "{value:?} vs {other:?}"
            );
        }
    }
}

/// Multi-byte boundaries and cached metadata agree with Unicode scalar iteration.
#[test]
fn unicode_index_and_cached_metadata_match_owned() {
    for text in ["", "ASCII", "é中🦀a", "🦀🦀🦀", "a\0é"] {
        let value = Value::string_utf8_from_bytes(text.as_bytes().to_vec()).unwrap();
        let expected = TypeSignature::type_of(&value).unwrap();
        let shared = packed(&value, &expected);
        let characters: Vec<_> = text.chars().map(|c| c.to_string().into_bytes()).collect();
        for _ in 0..3 {
            assert_eq!(shared.utf8_len(), Some(characters.len()));
            assert_eq!(shared.logical_size().unwrap(), value.size().unwrap());
            assert_eq!(shared.logical_type().unwrap(), expected);
        }
        for i in 0..=characters.len() {
            assert_eq!(
                shared.as_view().utf8_element(i),
                characters.get(i).map(Vec::as_slice)
            );
        }
        assert_eq!(shared.as_view().utf8_element(usize::MAX), None);
    }
}

/// Access to short lane elements does not require the widest element to come first.
#[test]
fn integer_and_boolean_lanes_match_every_element() {
    for values in [
        vec![Value::UInt(1); 1023]
            .into_iter()
            .chain([Value::UInt(u128::MAX)])
            .collect::<Vec<_>>(),
        vec![Value::Int(-1); 1023]
            .into_iter()
            .chain([Value::Int(i128::MIN)])
            .collect(),
        (0..1023).map(|i| Value::Bool(i % 3 == 0)).collect(),
    ] {
        let value = Value::list_from(values.clone()).unwrap();
        let shared = packed(&value, &TypeSignature::type_of(&value).unwrap());
        let list = shared.as_view().as_list().unwrap();
        for (i, expected) in values.iter().enumerate() {
            let element = list.get(i).unwrap().unwrap();
            assert_eq!(element.to_owned_value().unwrap(), *expected);
            assert_eq!(
                element.consensus_byte_len() as usize,
                expected.serialize_to_vec().unwrap().len()
            );
        }
        assert!(list.get(values.len()).unwrap().is_none());
    }
}

/// Shared fixed layouts remain correct after a copy-on-write schema merge.
#[test]
fn fixed_layout_cache_invalidates_on_merge() {
    let mut schema =
        TupleTypeSignature::try_from(vec![("z".try_into().unwrap(), TypeSignature::BoolType)])
            .unwrap();
    let original = schema.clone();
    assert_eq!(schema.packed_field_range(0), Some(0..1));
    let mut update =
        TupleTypeSignature::try_from(vec![("a".try_into().unwrap(), TypeSignature::BoolType)])
            .unwrap();
    assert_eq!(update.packed_fixed_width(), Some(1));
    schema.shallow_merge(&mut update);
    assert_eq!(schema.packed_fixed_width(), Some(2));
    assert_eq!(schema.packed_field_range(1), Some(1..2));
    assert_eq!(original.packed_fixed_width(), Some(1));
    assert_eq!(original.packed_field_range(1), None);
    assert_eq!(update.packed_fixed_width(), Some(0));
    let mut variable =
        TupleTypeSignature::try_from(vec![("b".try_into().unwrap(), TypeSignature::UIntType)])
            .unwrap();
    schema.shallow_merge(&mut variable);
    assert_eq!(schema.packed_fixed_width(), None);
}

/// Callable equality retains trait identity even though it is absent from packed bytes.
#[test]
fn callable_comparison_preserves_trait_metadata() {
    let contract = QualifiedContractIdentifier::transient();
    let traits: Vec<_> = ["first", "second"]
        .map(|name| TraitIdentifier {
            name: name.try_into().unwrap(),
            contract_identifier: contract.clone(),
        })
        .into();
    let values: Vec<_> = traits
        .iter()
        .map(|identifier| {
            Value::CallableContract(CallableData {
                contract_identifier: contract.clone(),
                trait_identifier: Some(Box::new(identifier.clone())),
            })
        })
        .collect();
    let schemas: Vec<_> = traits
        .iter()
        .map(|identifier| TypeSignature::CallableType(CallableSubtype::Trait(identifier.clone())))
        .collect();
    let first = packed(&values[0], &schemas[0]);
    let second = packed(&values[1], &schemas[1]);
    assert!(first.as_view().value_eq_owned(&values[0]).unwrap());
    assert!(!first.as_view().value_eq_owned(&values[1]).unwrap());
    assert!(!first.as_view().value_eq(second.as_view()).unwrap());
    let trait_reference = packed(
        &values[0],
        &TypeSignature::TraitReferenceType(traits[0].clone()),
    );
    assert!(first.as_view().value_eq(trait_reference.as_view()).unwrap());
}
