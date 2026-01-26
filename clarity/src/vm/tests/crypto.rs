use ark_bn254::{Bn254, Fq, Fr};
use ark_ff::{BigInteger, PrimeField};
use ark_groth16::Groth16;
use ark_relations::r1cs::{
    ConstraintSynthesizer, ConstraintSystemRef, LinearCombination, SynthesisError,
};
use ark_serialize::CanonicalSerialize;
use ark_snark::SNARK;
use ark_std::rand::rngs::OsRng;
use clarity_types::errors::CheckErrorKind;
use clarity_types::VmExecutionError;
use jf_relation::Arithmetization as _;
use proptest::prelude::*;
use stacks_common::types::chainstate::{StacksPrivateKey, StacksPublicKey};
use stacks_common::types::{PrivateKey, StacksEpochId};
use stacks_common::util::hash::{to_hex, Sha256Sum};
use stacks_common::util::secp256k1::MessageSignature as Secp256k1Signature;
use stacks_common::util::secp256r1::{Secp256r1PrivateKey, Secp256r1PublicKey};

use crate::vm::types::{ResponseData, TypeSignature, Value};
use crate::vm::{execute_with_parameters, ClarityVersion};

fn secp256r1_vectors() -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let privk = Secp256r1PrivateKey::from_seed(&[7u8; 32]);
    let pubk = Secp256r1PublicKey::from_private(&privk);
    let message_hash = Sha256Sum::from_data(b"clarity-secp256r1-tests");
    let signature = privk
        .sign(message_hash.as_bytes())
        .expect("secp256r1 signing should succeed");

    (
        message_hash.as_bytes().to_vec(),
        signature.0.to_vec(),
        pubk.to_bytes_compressed(),
    )
}

fn secp256k1_vectors() -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let privk = StacksPrivateKey::from_seed(&[9u8; 32]);
    let pubk = StacksPublicKey::from_private(&privk);
    let message_hash = Sha256Sum::from_data(b"clarity-secp256k1-tests");
    let signature: Secp256k1Signature = privk
        .sign(message_hash.as_bytes())
        .expect("secp256k1 signing should succeed");
    // Clarity expects R || S || v ordering.
    let signature_bytes = signature.to_rsv();

    (
        message_hash.as_bytes().to_vec(),
        signature_bytes,
        pubk.to_bytes_compressed(),
    )
}

fn buff_literal(bytes: &[u8]) -> String {
    format!("0x{}", to_hex(bytes))
}

fn zeroed_buff_literal(len: usize) -> String {
    buff_literal(&vec![0u8; len])
}

/// Helper to build a tiny TurboPlonk proof/vk/inputs (BN254)
fn make_minimal_plonk_proof_bytes() -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    use ark_bn254::{Bn254, Fr};
    use ark_serialize::CanonicalSerialize;
    use jf_plonk::proof_system::{PlonkKzgSnark, UniversalSNARK};
    use jf_plonk::transcript::StandardTranscript;
    use jf_relation::{Circuit, PlonkCircuit};
    use jf_utils::test_rng;

    // Build a tiny circuit with two public inputs:
    // public inputs: b = 1, f = 8
    // checks: a=3, b boolean, e=(a+b)*(a-b)=8
    let mut circuit = PlonkCircuit::<Fr>::new_turbo_plonk();
    let a = circuit.create_variable(Fr::from(3u32)).unwrap();
    let b = circuit.create_public_variable(Fr::from(1u32)).unwrap();
    circuit.enforce_constant(a, Fr::from(3u32)).unwrap();
    circuit.enforce_bool(b).unwrap();

    let c = circuit.add(a, b).unwrap();
    let d = circuit.sub(a, b).unwrap();
    let e = circuit.mul(c, d).unwrap();

    let f = circuit.create_public_variable(Fr::from(8u32)).unwrap();
    circuit.enforce_equal(e, f).unwrap();

    circuit.finalize_for_arithmetization().unwrap();

    let mut rng = test_rng();

    // For tests: requires jf-plonk with "test-srs" feature enabled.
    let srs_size = circuit.srs_size().unwrap();
    let srs = PlonkKzgSnark::<Bn254>::universal_setup_for_testing(srs_size, &mut rng).unwrap();

    let (pk, vk) = PlonkKzgSnark::<Bn254>::preprocess(&srs, &circuit).unwrap();
    let proof =
        PlonkKzgSnark::<Bn254>::prove::<_, _, StandardTranscript>(&mut rng, &circuit, &pk, None)
            .unwrap();

    let public_inputs = circuit.public_input().unwrap();

    // Serialize proof + vk compressed
    let mut proof_bytes = Vec::new();
    proof.serialize_compressed(&mut proof_bytes).unwrap();

    let mut vk_bytes = Vec::new();
    vk.serialize_compressed(&mut vk_bytes).unwrap();

    // Serialize public inputs as concatenated 32-byte Frs
    let mut inputs_bytes = Vec::new();
    for fr in public_inputs {
        fr.serialize_compressed(&mut inputs_bytes).unwrap();
    }

    (proof_bytes, vk_bytes, inputs_bytes)
}

#[derive(Clone)]
struct MulCircuit {
    a: Fr,
    b: Fr,
}

impl ConstraintSynthesizer<Fr> for MulCircuit {
    fn generate_constraints(self, cs: ConstraintSystemRef<Fr>) -> Result<(), SynthesisError> {
        let a_var = cs.new_input_variable(|| Ok(self.a))?;
        let b_var = cs.new_input_variable(|| Ok(self.b))?;
        let c = self.a * self.b;
        let c_var = cs.new_witness_variable(|| Ok(c))?;

        cs.enforce_constraint(
            LinearCombination::from(a_var),
            LinearCombination::from(b_var),
            LinearCombination::from(c_var),
        )?;
        Ok(())
    }
}

fn fq_to_be32(f: &Fq) -> [u8; 32] {
    let mut out = [0u8; 32];
    let bytes = f.into_bigint().to_bytes_be();
    out[32 - bytes.len()..].copy_from_slice(&bytes);
    out
}

/// Helper to build a tiny Groth16 proof/vk/inputs (BN254), with EVM-style proof encoding
fn make_minimal_groth16_proof_bytes() -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let mut rng = OsRng;

    // public inputs: a = 3, b = 5
    let a = Fr::from(3u32);
    let b = Fr::from(5u32);

    let circuit = MulCircuit { a, b };

    let (pk, vk) = Groth16::<Bn254>::circuit_specific_setup(circuit.clone(), &mut rng).unwrap();
    let proof = Groth16::<Bn254>::prove(&pk, circuit, &mut rng).unwrap();

    // vk bytes (arkworks compressed) for PoC
    let mut vk_bytes = Vec::new();
    vk.serialize_compressed(&mut vk_bytes).unwrap();

    // proof bytes in EVM-like layout:
    // [A.x, A.y, B.x_im, B.x_re, B.y_im, B.y_re, C.x, C.y]
    let mut proof_bytes = Vec::with_capacity(256);
    proof_bytes.extend_from_slice(&fq_to_be32(&proof.a.x));
    proof_bytes.extend_from_slice(&fq_to_be32(&proof.a.y));
    proof_bytes.extend_from_slice(&fq_to_be32(&proof.b.x.c1)); // x_im
    proof_bytes.extend_from_slice(&fq_to_be32(&proof.b.x.c0)); // x_re
    proof_bytes.extend_from_slice(&fq_to_be32(&proof.b.y.c1)); // y_im
    proof_bytes.extend_from_slice(&fq_to_be32(&proof.b.y.c0)); // y_re
    proof_bytes.extend_from_slice(&fq_to_be32(&proof.c.x));
    proof_bytes.extend_from_slice(&fq_to_be32(&proof.c.y));

    // public inputs: big-endian 32-byte Fr elements
    let mut inputs_bytes = Vec::new();
    inputs_bytes.extend_from_slice(&a.into_bigint().to_bytes_be().as_slice());
    if inputs_bytes.len() < 32 {
        let mut padded = vec![0u8; 32 - inputs_bytes.len()];
        padded.extend_from_slice(&inputs_bytes);
        inputs_bytes = padded;
    }

    let mut b_bytes = b.into_bigint().to_bytes_be();
    if b_bytes.len() < 32 {
        let mut padded = vec![0u8; 32 - b_bytes.len()];
        padded.append(&mut b_bytes);
        b_bytes = padded;
    }
    inputs_bytes.extend_from_slice(&b_bytes);

    (proof_bytes, vk_bytes, inputs_bytes)
}

#[test]
fn test_secp256r1_verify_valid_signature_returns_true() {
    let (message, signature, pubkey) = secp256r1_vectors();
    let program = format!(
        "(secp256r1-verify {} {} {})",
        buff_literal(&message),
        buff_literal(&signature),
        buff_literal(&pubkey)
    );

    assert_eq!(
        Value::Bool(true),
        execute_with_parameters(
            program.as_str(),
            ClarityVersion::Clarity4,
            StacksEpochId::Epoch33,
            false
        )
        .expect("execution should succeed")
        .expect("should return a value")
    );
}

#[test]
fn test_secp256r1_verify_valid_high_s_signature_returns_true() {
    let message = "0xc3abef6a775793dfbc8e0719e7a1de1fc2f90d37a7912b1ce8e300a5a03b06a8";
    let signature = "0xf2b8c0645caa7250e3b96d633cf40a88456e4ffbddffb69200c4e019039dfd31f153a6d5c3dc192a5574f3a261b1b70570971b92d8ebf86c17b7670d13591c4e";
    let pubkey = "0x031e18532fd4754c02f3041d9c75ceb33b83ffd81ac7ce4fe882ccb1c98bc5896e";

    let program = format!("(secp256r1-verify {message} {signature} {pubkey})");

    assert_eq!(
        Value::Bool(true),
        execute_with_parameters(
            program.as_str(),
            ClarityVersion::Clarity4,
            StacksEpochId::Epoch33,
            false
        )
        .expect("execution should succeed")
        .expect("should return a value")
    );
}

#[test]
fn test_secp256r1_verify_invalid_signature_returns_false() {
    let (message, mut signature, pubkey) = secp256r1_vectors();
    signature[0] ^= 0x01;

    let program = format!(
        "(secp256r1-verify {} {} {})",
        buff_literal(&message),
        buff_literal(&signature),
        buff_literal(&pubkey)
    );

    assert_eq!(
        Value::Bool(false),
        execute_with_parameters(
            program.as_str(),
            ClarityVersion::Clarity4,
            StacksEpochId::Epoch33,
            false
        )
        .expect("execution should succeed")
        .expect("should return a value")
    );
}

#[test]
fn test_secp256r1_verify_signature_too_short_returns_false() {
    let (message, mut signature, pubkey) = secp256r1_vectors();
    signature.truncate(63);

    let program = format!(
        "(secp256r1-verify {} {} {})",
        buff_literal(&message),
        buff_literal(&signature),
        buff_literal(&pubkey)
    );

    assert_eq!(
        Value::Bool(false),
        execute_with_parameters(
            program.as_str(),
            ClarityVersion::Clarity4,
            StacksEpochId::Epoch33,
            false
        )
        .expect("execution should succeed")
        .expect("should return a value")
    );
}

#[test]
fn test_secp256r1_verify_signature_too_long_errors() {
    let (message, mut signature, pubkey) = secp256r1_vectors();
    signature.push(0x00);

    let program = format!(
        "(secp256r1-verify {} {} {})",
        buff_literal(&message),
        buff_literal(&signature),
        buff_literal(&pubkey)
    );

    let err = execute_with_parameters(
        program.as_str(),
        ClarityVersion::Clarity4,
        StacksEpochId::Epoch33,
        false,
    )
    .unwrap_err();
    match err {
        VmExecutionError::Unchecked(CheckErrorKind::TypeValueError(expected, _)) => {
            assert_eq!(*expected, TypeSignature::BUFFER_64);
        }
        _ => panic!("expected BUFFER_65 type error, found {err:?}"),
    }
}

#[test]
fn test_secp256k1_verify_valid_signature_returns_true() {
    let (message, signature, pubkey) = secp256k1_vectors();
    let program = format!(
        "(secp256k1-verify {} {} {})",
        buff_literal(&message),
        buff_literal(&signature),
        buff_literal(&pubkey)
    );

    println!("program: {program}");

    assert_eq!(
        Value::Bool(true),
        execute_with_parameters(
            program.as_str(),
            ClarityVersion::Clarity4,
            StacksEpochId::Epoch33,
            false
        )
        .expect("execution should succeed")
        .expect("should return a value")
    );
}

#[test]
fn test_secp256k1_verify_valid_high_s_signature_returns_false() {
    let message = "0x89171d7815da4bc1f644665a3234bc99d1680afa0b3285eff4878f4275fbfa89";
    let signature = "0x54cd3f378a424a3e50ff1c911b7d80cf424e1b86dddecadbcf39077e62fa1e54ee6514347c1608df2c3995e7356f2d60a1fab60878214642134d78cd923ce27a01";
    let pubkey = "0x0256b328b30c8bf5839e24058747879408bdb36241dc9c2e7c619faa12b2920967";

    let program = format!("(secp256k1-verify {message} {signature} {pubkey})");

    assert_eq!(
        Value::Bool(false),
        execute_with_parameters(
            program.as_str(),
            ClarityVersion::Clarity4,
            StacksEpochId::Epoch33,
            false
        )
        .expect("execution should succeed")
        .expect("should return a value")
    );
}

#[test]
fn test_secp256k1_verify_invalid_signature_returns_false() {
    let (message, mut signature, pubkey) = secp256k1_vectors();
    signature[10] ^= 0x01;

    let program = format!(
        "(secp256k1-verify {} {} {})",
        buff_literal(&message),
        buff_literal(&signature),
        buff_literal(&pubkey)
    );

    assert_eq!(
        Value::Bool(false),
        execute_with_parameters(
            program.as_str(),
            ClarityVersion::Clarity4,
            StacksEpochId::Epoch33,
            false
        )
        .expect("execution should succeed")
        .expect("should return a value")
    );
}

#[test]
fn test_secp256k1_verify_signature_too_short_returns_false() {
    let (message, mut signature, pubkey) = secp256k1_vectors();
    signature.truncate(63);

    let program = format!(
        "(secp256k1-verify {} {} {})",
        buff_literal(&message),
        buff_literal(&signature),
        buff_literal(&pubkey)
    );

    assert_eq!(
        Value::Bool(false),
        execute_with_parameters(
            program.as_str(),
            ClarityVersion::Clarity4,
            StacksEpochId::Epoch33,
            false
        )
        .expect("execution should succeed")
        .expect("should return a value")
    );
}

#[test]
fn test_secp256k1_verify_recovery_id_out_of_range_returns_false() {
    let (message, mut signature, pubkey) = secp256k1_vectors();
    if let Some(last) = signature.last_mut() {
        *last = 0x04;
    }

    let program = format!(
        "(secp256k1-verify {} {} {})",
        buff_literal(&message),
        buff_literal(&signature),
        buff_literal(&pubkey)
    );

    assert_eq!(
        Value::Bool(false),
        execute_with_parameters(
            program.as_str(),
            ClarityVersion::Clarity4,
            StacksEpochId::Epoch33,
            false
        )
        .expect("execution should succeed")
        .expect("should return a value")
    );
}

#[test]
fn test_secp256k1_verify_signature_too_long_errors() {
    let (message, mut signature, pubkey) = secp256k1_vectors();
    signature.extend([0x00, 0x01]);

    let program = format!(
        "(secp256k1-verify {} {} {})",
        buff_literal(&message),
        buff_literal(&signature),
        buff_literal(&pubkey)
    );

    let err = execute_with_parameters(
        program.as_str(),
        ClarityVersion::Clarity4,
        StacksEpochId::Epoch33,
        false,
    )
    .unwrap_err();
    match err {
        VmExecutionError::Unchecked(CheckErrorKind::TypeValueError(expected, _)) => {
            assert_eq!(*expected, TypeSignature::BUFFER_65);
        }
        _ => panic!("expected BUFFER_65 type error, found {err:?}"),
    }
}

#[test]
fn test_secp256k1_recover_returns_expected_public_key() {
    let (message, signature, pubkey) = secp256k1_vectors();
    let fallback = zeroed_buff_literal(33);
    let program = format!(
        "(is-eq (unwrap! (secp256k1-recover? {} {}) {}) {})",
        buff_literal(&message),
        buff_literal(&signature),
        fallback,
        buff_literal(&pubkey)
    );

    assert_eq!(
        Value::Bool(true),
        execute_with_parameters(
            program.as_str(),
            ClarityVersion::Clarity4,
            StacksEpochId::Epoch33,
            false
        )
        .expect("execution should succeed")
        .expect("should return a value")
    );
}

#[test]
fn test_secp256k1_recover_invalid_signature_returns_err_code() {
    let (message, mut signature, _pubkey) = secp256k1_vectors();
    signature[5] ^= 0x02;

    let program = format!(
        "(secp256k1-recover? {} {})",
        buff_literal(&message),
        buff_literal(&signature)
    );

    match execute_with_parameters(
        program.as_str(),
        ClarityVersion::Clarity4,
        StacksEpochId::Epoch33,
        false,
    )
    .expect("execution should succeed")
    .expect("should return a value")
    {
        Value::Response(ResponseData { data, .. }) => {
            assert_eq!(data, Box::new(Value::UInt(1)));
        }
        other => panic!("expected err response, found {other:?}"),
    }
}

#[test]
fn test_plonk_verify_valid_proof_returns_true() {
    let (proof_bytes, vk_bytes, inputs_bytes) = make_minimal_plonk_proof_bytes();

    let program = format!(
        "(plonk-verify {} {} {})",
        buff_literal(&proof_bytes),
        buff_literal(&vk_bytes),
        buff_literal(&inputs_bytes),
    );

    assert_eq!(
        Value::Bool(true),
        execute_with_parameters(
            program.as_str(),
            ClarityVersion::Clarity4,
            StacksEpochId::Epoch33,
            false
        )
        .expect("execution should succeed")
        .expect("should return a value")
    );
}

#[test]
fn test_plonk_verify_invalid_proof_returns_false() {
    let (proof_bytes, vk_bytes, mut inputs_bytes) = make_minimal_plonk_proof_bytes();

    // Corrupt public inputs (fixed-size) to avoid proof deserialization panics.
    if !inputs_bytes.is_empty() {
        inputs_bytes[0] ^= 0x01;
    }

    let program = format!(
        "(plonk-verify {} {} {})",
        buff_literal(&proof_bytes),
        buff_literal(&vk_bytes),
        buff_literal(&inputs_bytes),
    );

    assert_eq!(
        Value::Bool(false),
        execute_with_parameters(
            program.as_str(),
            ClarityVersion::Clarity4,
            StacksEpochId::Epoch33,
            false
        )
        .expect("execution should succeed")
        .expect("should return a value")
    );
}

#[test]
fn test_groth16_verify_valid_proof_returns_true() {
    let (proof_bytes, vk_bytes, inputs_bytes) = make_minimal_groth16_proof_bytes();

    let program = format!(
        "(groth16-verify {} {} {})",
        buff_literal(&proof_bytes),
        buff_literal(&vk_bytes),
        buff_literal(&inputs_bytes),
    );

    assert_eq!(
        Value::Bool(true),
        execute_with_parameters(
            program.as_str(),
            ClarityVersion::Clarity4,
            StacksEpochId::Epoch33,
            false
        )
        .expect("execution should succeed")
        .expect("should return a value")
    );
}

#[test]
fn test_groth16_verify_invalid_proof_returns_false() {
    let (proof_bytes, vk_bytes, mut inputs_bytes) = make_minimal_groth16_proof_bytes();

    // Corrupt public inputs to force failure
    if !inputs_bytes.is_empty() {
        inputs_bytes[0] ^= 0x01;
    }

    let program = format!(
        "(groth16-verify {} {} {})",
        buff_literal(&proof_bytes),
        buff_literal(&vk_bytes),
        buff_literal(&inputs_bytes),
    );

    assert_eq!(
        Value::Bool(false),
        execute_with_parameters(
            program.as_str(),
            ClarityVersion::Clarity4,
            StacksEpochId::Epoch33,
            false
        )
        .expect("execution should succeed")
        .expect("should return a value")
    );
}

proptest! {
    #[test]
    fn prop_secp256k1_verify_accepts_valid_signatures(
        seed in any::<[u8; 32]>(),
        message in any::<[u8; 32]>()
    ) {
        let privk = StacksPrivateKey::from_seed(&seed);
        let pubk = StacksPublicKey::from_private(&privk);
        let pubkey_bytes = pubk.to_bytes_compressed();
        let message = message.to_vec();
        let signature: Secp256k1Signature = privk.sign(&message).expect("secp256k1 signing should succeed");
        let signature_bytes = signature.to_rsv();
        let program = format!(
            "(secp256k1-verify {} {} {})",
            buff_literal(&message),
            buff_literal(&signature_bytes),
            buff_literal(&pubkey_bytes)
        );

        let result = execute_with_parameters(
            program.as_str(),
            ClarityVersion::Clarity4,
            StacksEpochId::Epoch33,
            false,
        )
        .expect("execution should succeed")
        .expect("should return a value");

        prop_assert_eq!(Value::Bool(true), result);
    }

    #[test]
    fn prop_secp256k1_recover_matches_public_key(
        seed in any::<[u8; 32]>(),
        message in any::<[u8; 32]>()
    ) {
        let privk = StacksPrivateKey::from_seed(&seed);
        let pubk = StacksPublicKey::from_private(&privk);
        let pubkey_bytes = pubk.to_bytes_compressed();
        let message = message.to_vec();
        let signature: Secp256k1Signature = privk.sign(&message).expect("secp256k1 signing should succeed");
        let signature_bytes = signature.to_rsv();
        let program = format!(
            "(is-eq (unwrap! (secp256k1-recover? {} {}) (err u1)) {})",
            buff_literal(&message),
            buff_literal(&signature_bytes),
            buff_literal(&pubkey_bytes)
        );

        let result = execute_with_parameters(
            program.as_str(),
            ClarityVersion::Clarity4,
            StacksEpochId::Epoch33,
            false,
        )
        .expect("execution should succeed")
        .expect("should return a value");

        prop_assert_eq!(Value::Bool(true), result);
    }

    #[test]
    fn prop_secp256r1_verify_accepts_valid_signatures(
        seed in any::<[u8; 32]>(),
        message in any::<[u8; 32]>()
    ) {
        let privk = Secp256r1PrivateKey::from_seed(&seed);
        let pubk = Secp256r1PublicKey::from_private(&privk);
        let pubkey_bytes = pubk.to_bytes_compressed();
        let message = message.to_vec();
        let signature = privk.sign(&message).expect("secp256r1 signing should succeed");
        let program = format!(
            "(secp256r1-verify {} {} {})",
            buff_literal(&message),
            buff_literal(&signature.0),
            buff_literal(&pubkey_bytes)
        );

        let result = execute_with_parameters(
            program.as_str(),
            ClarityVersion::Clarity4,
            StacksEpochId::Epoch33,
            false,
        )
        .expect("execution should succeed")
        .expect("should return a value");

        prop_assert_eq!(Value::Bool(true), result);
    }

    #[test]
    fn prop_secp256k1_verify_rejects_tampered_msg(
        seed in any::<[u8; 32]>(),
        message in any::<[u8; 32]>(),
        bit in 0usize..32
    ) {
        let privk = StacksPrivateKey::from_seed(&seed);
        let pubk = StacksPublicKey::from_private(&privk);
        let pubkey_bytes = pubk.to_bytes_compressed();
        let mut m = message.to_vec();
        let sig: Secp256k1Signature = privk.sign(&m).unwrap();
        let sig_bytes = sig.to_rsv();

        // flip one bit
        m[bit] ^= 0x01;

        let program = format!(
            "(secp256k1-verify {} {} {})",
            buff_literal(&m),
            buff_literal(&sig_bytes),
            buff_literal(&pubkey_bytes)
        );
        let result = execute_with_parameters(
            &program, ClarityVersion::Clarity4, StacksEpochId::Epoch33, false
        ).unwrap().unwrap();

        prop_assert_eq!(Value::Bool(false), result);
    }

    #[test]
    fn prop_secp256r1_verify_rejects_tampered_msg(
        seed in any::<[u8; 32]>(),
        message in any::<[u8; 32]>(),
        bit in 0usize..32
    ) {
        let privk = Secp256r1PrivateKey::from_seed(&seed);
        let pubk = Secp256r1PublicKey::from_private(&privk);
        let pubkey_bytes = pubk.to_bytes_compressed();
        let mut message = message.to_vec();
        let signature = privk.sign(&message).expect("secp256r1 signing should succeed");

        // flip one bit
        message[bit] ^= 0x01;

        let program = format!(
            "(secp256r1-verify {} {} {})",
            buff_literal(&message),
            buff_literal(&signature.0),
            buff_literal(&pubkey_bytes)
        );

        let result = execute_with_parameters(
            program.as_str(),
            ClarityVersion::Clarity4,
            StacksEpochId::Epoch33,
            false,
        )
        .expect("execution should succeed")
        .expect("should return a value");

        prop_assert_eq!(Value::Bool(false), result);
    }

    #[test]
    fn prop_secp256k1_recover_fails_to_match_with_tampered_msg(
        seed in any::<[u8; 32]>(),
        message in any::<[u8; 32]>(),
        bit in 0usize..32
    ) {
        let privk = StacksPrivateKey::from_seed(&seed);
        let pubk = StacksPublicKey::from_private(&privk);
        let pubkey_bytes = pubk.to_bytes_compressed();
        let mut message = message.to_vec();
        let signature: Secp256k1Signature = privk.sign(&message).expect("secp256k1 signing should succeed");
        let signature_bytes = signature.to_rsv();

        // flip one bit
        message[bit] ^= 0x01;

        let program = format!(
            "(is-eq (unwrap! (secp256k1-recover? {} {}) (err u1)) {})",
            buff_literal(&message),
            buff_literal(&signature_bytes),
            buff_literal(&pubkey_bytes)
        );

        let result = execute_with_parameters(
            program.as_str(),
            ClarityVersion::Clarity4,
            StacksEpochId::Epoch33,
            false,
        )
        .expect("execution should succeed")
        .expect("should return a value");

        prop_assert_eq!(Value::Bool(false), result);
    }

    #[test]
    fn prop_secp256r1_verify_rejects_wrong_key(
        seed_a in any::<[u8; 32]>(),
        seed_b in any::<[u8; 32]>(),
        message in any::<[u8; 32]>()
    ) {
        prop_assume!(seed_a != seed_b);

        let priv_a = Secp256r1PrivateKey::from_seed(&seed_a);
        let pub_b  = Secp256r1PublicKey::from_private(&Secp256r1PrivateKey::from_seed(&seed_b));
        let pub_b_bytes = pub_b.to_bytes_compressed();

        let msg = message.to_vec();
        let signature = priv_a.sign(&msg).unwrap();

        let program = format!(
            "(secp256r1-verify {} {} {})",
            buff_literal(&msg),
            buff_literal(&signature.0),
            buff_literal(&pub_b_bytes)
        );
        let result = execute_with_parameters(
            &program, ClarityVersion::Clarity4, StacksEpochId::Epoch33, false
        ).unwrap().unwrap();

        prop_assert_eq!(Value::Bool(false), result);
    }

    #[test]
    fn prop_secp256k1_verify_rejects_wrong_key(
        seed_a in any::<[u8; 32]>(),
        seed_b in any::<[u8; 32]>(),
        message in any::<[u8; 32]>()
    ) {
        prop_assume!(seed_a != seed_b);
        let priv_a = StacksPrivateKey::from_seed(&seed_a);
        let pub_b  = StacksPublicKey::from_private(&StacksPrivateKey::from_seed(&seed_b));
        let pub_b_bytes = pub_b.to_bytes_compressed();

        let message = message.to_vec();
        let signature: Secp256k1Signature = priv_a.sign(&message).expect("secp256k1 signing should succeed");
        let signature_bytes = signature.to_rsv();
        let program = format!(
            "(secp256k1-verify {} {} {})",
            buff_literal(&message),
            buff_literal(&signature_bytes),
            buff_literal(&pub_b_bytes)
        );

        let result = execute_with_parameters(
            program.as_str(),
            ClarityVersion::Clarity4,
            StacksEpochId::Epoch33,
            false,
        )
        .expect("execution should succeed")
        .expect("should return a value");

        prop_assert_eq!(Value::Bool(false), result);
    }

    #[test]
    fn prop_secp256k1_recover_fails_to_match_with_wrong_key(
        seed_a in any::<[u8; 32]>(),
        seed_b in any::<[u8; 32]>(),
        message in any::<[u8; 32]>()
    ) {
        let priv_a = StacksPrivateKey::from_seed(&seed_a);
        let pub_b  = StacksPublicKey::from_private(&StacksPrivateKey::from_seed(&seed_b));
        let pub_b_bytes = pub_b.to_bytes_compressed();

        let message = message.to_vec();
        let signature: Secp256k1Signature = priv_a.sign(&message).expect("secp256k1 signing should succeed");
        let signature_bytes = signature.to_rsv();
        let program = format!(
            "(is-eq (unwrap! (secp256k1-recover? {} {}) (err u1)) {})",
            buff_literal(&message),
            buff_literal(&signature_bytes),
            buff_literal(&pub_b_bytes)
        );

        let result = execute_with_parameters(
            program.as_str(),
            ClarityVersion::Clarity4,
            StacksEpochId::Epoch33,
            false,
        )
        .expect("execution should succeed")
        .expect("should return a value");

        prop_assert_eq!(Value::Bool(false), result);
    }
}
