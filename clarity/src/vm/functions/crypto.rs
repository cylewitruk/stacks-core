// Copyright (C) 2013-2020 Blockstack PBC, a public benefit corporation
// Copyright (C) 2020 Stacks Open Internet Foundation
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

use ark_bn254::{Bn254, Fq, Fq2, Fr, G1Affine, G2Affine};
use ark_ec::pairing::Pairing;
use ark_ec::{AffineRepr as _, CurveGroup as _, PrimeGroup as _};
use ark_ff::{BigInteger as _, One as _, PrimeField, Zero as _};
use ark_groth16::Groth16;
use ark_serialize::CanonicalDeserialize as _;
use jf_plonk::proof_system::{PlonkKzgSnark, UniversalSNARK};
use jf_plonk::transcript::StandardTranscript;
use stacks_common::address::{
    AddressHashMode, C32_ADDRESS_VERSION_MAINNET_SINGLESIG, C32_ADDRESS_VERSION_TESTNET_SINGLESIG,
};
use stacks_common::types::chainstate::StacksAddress;
use stacks_common::util::hash;
use stacks_common::util::secp256k1::{secp256k1_recover, secp256k1_verify, Secp256k1PublicKey};
use stacks_common::util::secp256r1::secp256r1_verify;

use crate::vm::costs::cost_functions::ClarityCostFunction;
use crate::vm::costs::runtime_cost;
use crate::vm::errors::{check_argument_count, CheckErrorKind, VmExecutionError, VmInternalError};
use crate::vm::representations::SymbolicExpression;
use crate::vm::types::{BuffData, SequenceData, TypeSignature, Value};
use crate::vm::{eval, ClarityVersion, Environment, LocalContext};

macro_rules! native_hash_func {
    ($name:ident, $module:ty) => {
        pub fn $name(input: Value) -> Result<Value, VmExecutionError> {
            let bytes = match input {
                Value::Int(value) => Ok(value.to_le_bytes().to_vec()),
                Value::UInt(value) => Ok(value.to_le_bytes().to_vec()),
                Value::Sequence(SequenceData::Buffer(value)) => Ok(value.data),
                _ => Err(CheckErrorKind::UnionTypeValueError(
                    vec![
                        TypeSignature::IntType,
                        TypeSignature::UIntType,
                        TypeSignature::BUFFER_MAX,
                    ],
                    Box::new(input),
                )),
            }?;
            let hash = <$module>::from_data(&bytes);
            Value::buff_from(hash.as_bytes().to_vec())
        }
    };
}

native_hash_func!(native_hash160, hash::Hash160);
native_hash_func!(native_sha256, hash::Sha256Sum);
native_hash_func!(native_sha512, hash::Sha512Sum);
native_hash_func!(native_sha512trunc256, hash::Sha512Trunc256Sum);
native_hash_func!(native_keccak256, hash::Keccak256Hash);

fn expect_buffer(value: Value, ty: TypeSignature) -> Result<Vec<u8>, VmExecutionError> {
    match value {
        Value::Sequence(SequenceData::Buffer(BuffData { data })) => Ok(data),
        _ => Err(CheckErrorKind::TypeValueError(Box::new(ty), Box::new(value)).into()),
    }
}

fn parse_fq_be(bytes: &[u8]) -> Option<Fq> {
    if bytes.len() != 32 {
        return None;
    }
    Some(Fq::from_be_bytes_mod_order(bytes))
}

fn parse_fr_be(bytes: &[u8]) -> Option<Fr> {
    if bytes.len() != 32 {
        return None;
    }
    Some(Fr::from_be_bytes_mod_order(bytes))
}

fn parse_g1_be(x: &[u8], y: &[u8]) -> Option<G1Affine> {
    let x = parse_fq_be(x)?;
    let y = parse_fq_be(y)?;
    if x.is_zero() && y.is_zero() {
        return Some(G1Affine::identity());
    }
    let p = G1Affine::new_unchecked(x, y);
    if !p.is_on_curve() {
        return None;
    }
    if !p.is_in_correct_subgroup_assuming_on_curve() {
        return None;
    }
    Some(p)
}

fn parse_g2_be(x_im: &[u8], x_re: &[u8], y_im: &[u8], y_re: &[u8]) -> Option<G2Affine> {
    let x = Fq2::new(parse_fq_be(x_re)?, parse_fq_be(x_im)?);
    let y = Fq2::new(parse_fq_be(y_re)?, parse_fq_be(y_im)?);
    if x.is_zero() && y.is_zero() {
        return Some(G2Affine::identity());
    }
    let p = G2Affine::new_unchecked(x, y);
    if !p.is_on_curve() {
        return None;
    }
    if !p.is_in_correct_subgroup_assuming_on_curve() {
        return None;
    }
    Some(p)
}

fn fq_to_be32(f: &Fq) -> [u8; 32] {
    let mut out = [0u8; 32];
    let bytes = f.into_bigint().to_bytes_be();
    out[32 - bytes.len()..].copy_from_slice(&bytes);
    out
}

fn g1_to_bytes(p: &G1Affine) -> Vec<u8> {
    if p.is_zero() {
        return vec![0u8; 64];
    }
    let mut out = Vec::with_capacity(64);
    out.extend_from_slice(&fq_to_be32(&p.x));
    out.extend_from_slice(&fq_to_be32(&p.y));
    out
}

fn g2_to_bytes(p: &G2Affine) -> Vec<u8> {
    if p.is_zero() {
        return vec![0u8; 128];
    }
    let mut out = Vec::with_capacity(128);
    // EVM order: x_im, x_re, y_im, y_re
    out.extend_from_slice(&fq_to_be32(&p.x.c1)); // x_im
    out.extend_from_slice(&fq_to_be32(&p.x.c0)); // x_re
    out.extend_from_slice(&fq_to_be32(&p.y.c1)); // y_im
    out.extend_from_slice(&fq_to_be32(&p.y.c0)); // y_re
    out
}

// Note: Clarity1 had a bug in how the address is computed (issues/2619).
// This method preserves the old, incorrect behavior for those running Clarity1.
fn pubkey_to_address_v1(pub_key: Secp256k1PublicKey) -> Result<StacksAddress, VmExecutionError> {
    StacksAddress::from_public_keys(
        C32_ADDRESS_VERSION_TESTNET_SINGLESIG,
        &AddressHashMode::SerializeP2PKH,
        1,
        &vec![pub_key],
    )
    .ok_or_else(|| VmInternalError::Expect("Failed to create address from pubkey".into()).into())
}

// Note: Clarity1 had a bug in how the address is computed (issues/2619).
// This version contains the code for Clarity2 and going forward.
fn pubkey_to_address_v2(
    pub_key: Secp256k1PublicKey,
    is_mainnet: bool,
) -> Result<StacksAddress, VmExecutionError> {
    let network_byte = if is_mainnet {
        C32_ADDRESS_VERSION_MAINNET_SINGLESIG
    } else {
        C32_ADDRESS_VERSION_TESTNET_SINGLESIG
    };
    StacksAddress::from_public_keys(
        network_byte,
        &AddressHashMode::SerializeP2PKH,
        1,
        &vec![pub_key],
    )
    .ok_or_else(|| VmInternalError::Expect("Failed to create address from pubkey".into()).into())
}

pub fn special_principal_of(
    args: &[SymbolicExpression],
    env: &mut Environment,
    context: &LocalContext,
) -> Result<Value, VmExecutionError> {
    // (principal-of? (..))
    // arg0 => (buff 33)
    check_argument_count(1, args)?;

    runtime_cost(ClarityCostFunction::PrincipalOf, env, 0)?;

    let param0 = eval(&args[0], env, context)?;
    let pub_key = match param0 {
        Value::Sequence(SequenceData::Buffer(BuffData { ref data })) => {
            if data.len() != 33 {
                return Err(CheckErrorKind::TypeValueError(
                    Box::new(TypeSignature::BUFFER_33),
                    Box::new(param0),
                )
                .into());
            }
            data
        }
        _ => {
            return Err(CheckErrorKind::TypeValueError(
                Box::new(TypeSignature::BUFFER_33),
                Box::new(param0),
            )
            .into())
        }
    };

    if let Ok(pub_key) = Secp256k1PublicKey::from_slice(pub_key) {
        // Note: Clarity1 had a bug in how the address is computed (issues/2619).
        // We want to preserve the old behavior unless the version is greater.
        let addr = if *env.contract_context.get_clarity_version() > ClarityVersion::Clarity1 {
            pubkey_to_address_v2(pub_key, env.global_context.mainnet)?
        } else {
            pubkey_to_address_v1(pub_key)?
        };
        let principal = addr.into();
        Ok(Value::okay(Value::Principal(principal))
            .map_err(|_| VmInternalError::Expect("Failed to construct ok".into()))?)
    } else {
        Ok(Value::err_uint(1))
    }
}

pub fn special_secp256k1_recover(
    args: &[SymbolicExpression],
    env: &mut Environment,
    context: &LocalContext,
) -> Result<Value, VmExecutionError> {
    // (secp256k1-recover? (..))
    // arg0 => (buff 32), arg1 => (buff 65)
    check_argument_count(2, args)?;

    runtime_cost(ClarityCostFunction::Secp256k1recover, env, 0)?;

    let param0 = eval(&args[0], env, context)?;
    let message = match param0 {
        Value::Sequence(SequenceData::Buffer(BuffData { ref data })) => {
            if data.len() != 32 {
                return Err(CheckErrorKind::TypeValueError(
                    Box::new(TypeSignature::BUFFER_32),
                    Box::new(param0),
                )
                .into());
            }
            data
        }
        _ => {
            return Err(CheckErrorKind::TypeValueError(
                Box::new(TypeSignature::BUFFER_32),
                Box::new(param0),
            )
            .into())
        }
    };

    let param1 = eval(&args[1], env, context)?;
    let signature = match param1 {
        Value::Sequence(SequenceData::Buffer(BuffData { ref data })) => {
            if data.len() > 65 {
                return Err(CheckErrorKind::TypeValueError(
                    Box::new(TypeSignature::BUFFER_65),
                    Box::new(param1),
                )
                .into());
            }
            if data.len() < 65 || data[64] > 3 {
                return Ok(Value::err_uint(2));
            }
            data
        }
        _ => {
            return Err(CheckErrorKind::TypeValueError(
                Box::new(TypeSignature::BUFFER_65),
                Box::new(param1),
            )
            .into())
        }
    };

    let Ok(pubkey) = secp256k1_recover(message, signature) else {
        // We do not return the runtime error. Immediately map this to an error code.
        return Ok(Value::err_uint(1));
    };
    let pubkey_buff = Value::buff_from(pubkey.to_vec())
        .map_err(|_| VmInternalError::Expect("Failed to construct buff".into()))?;
    Ok(Value::okay(pubkey_buff)
        .map_err(|_| VmInternalError::Expect("Failed to construct ok".into()))?)
}

pub fn special_secp256k1_verify(
    args: &[SymbolicExpression],
    env: &mut Environment,
    context: &LocalContext,
) -> Result<Value, VmExecutionError> {
    // (secp256k1-verify (..))
    // arg0 => (buff 32), arg1 => (buff 65), arg2 => (buff 33)
    check_argument_count(3, args)?;

    runtime_cost(ClarityCostFunction::Secp256k1verify, env, 0)?;

    let param0 = eval(&args[0], env, context)?;
    let message = match param0 {
        Value::Sequence(SequenceData::Buffer(BuffData { ref data })) => {
            if data.len() != 32 {
                return Err(CheckErrorKind::TypeValueError(
                    Box::new(TypeSignature::BUFFER_32),
                    Box::new(param0),
                )
                .into());
            }
            data
        }
        _ => {
            return Err(CheckErrorKind::TypeValueError(
                Box::new(TypeSignature::BUFFER_32),
                Box::new(param0),
            )
            .into())
        }
    };

    let param1 = eval(&args[1], env, context)?;
    let signature = match param1 {
        Value::Sequence(SequenceData::Buffer(BuffData { ref data })) => {
            if data.len() > 65 {
                return Err(CheckErrorKind::TypeValueError(
                    Box::new(TypeSignature::BUFFER_65),
                    Box::new(param1),
                )
                .into());
            }
            if data.len() < 64 {
                return Ok(Value::Bool(false));
            }
            if data.len() == 65 && data[64] > 3 {
                return Ok(Value::Bool(false));
            }
            data
        }
        _ => {
            return Err(CheckErrorKind::TypeValueError(
                Box::new(TypeSignature::BUFFER_65),
                Box::new(param1),
            )
            .into())
        }
    };

    let param2 = eval(&args[2], env, context)?;
    let pubkey = match param2 {
        Value::Sequence(SequenceData::Buffer(BuffData { ref data })) => {
            if data.len() != 33 {
                return Err(CheckErrorKind::TypeValueError(
                    Box::new(TypeSignature::BUFFER_33),
                    Box::new(param2),
                )
                .into());
            }
            data
        }
        _ => {
            return Err(CheckErrorKind::TypeValueError(
                Box::new(TypeSignature::BUFFER_33),
                Box::new(param2),
            )
            .into())
        }
    };

    Ok(Value::Bool(
        secp256k1_verify(message, signature, pubkey).is_ok(),
    ))
}

pub fn special_secp256r1_verify(
    args: &[SymbolicExpression],
    env: &mut Environment,
    context: &LocalContext,
) -> Result<Value, VmExecutionError> {
    // (secp256r1-verify message-hash signature public-key)
    // message-hash: (buff 32), signature: (buff 64), public-key: (buff 33)
    check_argument_count(3, args)?;

    runtime_cost(ClarityCostFunction::Secp256r1verify, env, 0)?;

    let arg0 = args
        .first()
        .ok_or(CheckErrorKind::IncorrectArgumentCount(0, 3))?;
    let message_value = eval(arg0, env, context)?;
    let message = match message_value {
        Value::Sequence(SequenceData::Buffer(BuffData { ref data })) => {
            if data.len() != 32 {
                return Err(CheckErrorKind::TypeValueError(
                    Box::new(TypeSignature::BUFFER_32),
                    Box::new(message_value),
                )
                .into());
            }
            data
        }
        _ => {
            return Err(CheckErrorKind::TypeValueError(
                Box::new(TypeSignature::BUFFER_32),
                Box::new(message_value),
            )
            .into())
        }
    };

    let arg1 = args
        .get(1)
        .ok_or(CheckErrorKind::IncorrectArgumentCount(1, 3))?;
    let signature_value = eval(arg1, env, context)?;
    let signature = match signature_value {
        Value::Sequence(SequenceData::Buffer(BuffData { ref data })) => {
            if data.len() > 64 {
                return Err(CheckErrorKind::TypeValueError(
                    Box::new(TypeSignature::BUFFER_64),
                    Box::new(signature_value),
                )
                .into());
            }
            if data.len() != 64 {
                return Ok(Value::Bool(false));
            }
            data
        }
        _ => {
            return Err(CheckErrorKind::TypeValueError(
                Box::new(TypeSignature::BUFFER_64),
                Box::new(signature_value),
            )
            .into())
        }
    };

    let arg2 = args
        .get(2)
        .ok_or(CheckErrorKind::IncorrectArgumentCount(2, 3))?;
    let pubkey_value = eval(arg2, env, context)?;
    let pubkey = match pubkey_value {
        Value::Sequence(SequenceData::Buffer(BuffData { ref data })) => {
            if data.len() != 33 {
                return Err(CheckErrorKind::TypeValueError(
                    Box::new(TypeSignature::BUFFER_33),
                    Box::new(pubkey_value),
                )
                .into());
            }
            data
        }
        _ => {
            return Err(CheckErrorKind::TypeValueError(
                Box::new(TypeSignature::BUFFER_33),
                Box::new(pubkey_value),
            )
            .into())
        }
    };

    Ok(Value::Bool(
        secp256r1_verify(message, signature, pubkey).is_ok(),
    ))
}

pub fn special_plonk_verify(
    args: &[SymbolicExpression],
    env: &mut Environment,
    context: &LocalContext,
) -> Result<Value, VmExecutionError> {
    // (plonk-verify proof vk public-inputs)
    check_argument_count(3, args)?;
    runtime_cost(ClarityCostFunction::PlonkVerify, env, 0)?;

    let proof_bytes = expect_buffer(eval(&args[0], env, context)?, TypeSignature::BUFFER_MAX)?;
    let vk_bytes = expect_buffer(eval(&args[1], env, context)?, TypeSignature::BUFFER_MAX)?;
    let inputs_bytes = expect_buffer(eval(&args[2], env, context)?, TypeSignature::BUFFER_MAX)?;

    let proof =
        jf_plonk::proof_system::structs::Proof::<Bn254>::deserialize_compressed(&*proof_bytes).ok();
    let vk =
        jf_plonk::proof_system::structs::VerifyingKey::<Bn254>::deserialize_compressed(&*vk_bytes)
            .ok();
    if proof.is_none() || vk.is_none() {
        return Ok(Value::Bool(false));
    }

    let proof = proof.unwrap();
    let vk = vk.unwrap();

    // public inputs: concatenated 32-byte chunks (BN254 Fr)
    if inputs_bytes.len() % 32 != 0 {
        return Ok(Value::Bool(false));
    }
    let mut inputs = Vec::new();
    for chunk in inputs_bytes.chunks(32) {
        let fr = Fr::deserialize_compressed(chunk).ok();
        if fr.is_none() {
            return Ok(Value::Bool(false));
        }
        inputs.push(fr.unwrap());
    }

    let ok =
        PlonkKzgSnark::<Bn254>::verify::<StandardTranscript>(&vk, &inputs, &proof, None).is_ok();

    Ok(Value::Bool(ok))
}

pub fn special_groth16_verify(
    args: &[SymbolicExpression],
    env: &mut Environment,
    context: &LocalContext,
) -> Result<Value, VmExecutionError> {
    // (groth16-verify proof vk public-inputs)
    check_argument_count(3, args)?;
    runtime_cost(ClarityCostFunction::Groth16Verify, env, 0)?;

    let proof_bytes = expect_buffer(eval(&args[0], env, context)?, TypeSignature::BUFFER_MAX)?;
    let vk_bytes = expect_buffer(eval(&args[1], env, context)?, TypeSignature::BUFFER_MAX)?;
    let inputs_bytes = expect_buffer(eval(&args[2], env, context)?, TypeSignature::BUFFER_MAX)?;

    if proof_bytes.len() != 256 {
        return Ok(Value::Bool(false));
    }

    // Proof decoding (A, B, C)
    let a = parse_g1_be(&proof_bytes[0..32], &proof_bytes[32..64]);
    let b = parse_g2_be(
        &proof_bytes[64..96],   // x_im
        &proof_bytes[96..128],  // x_re
        &proof_bytes[128..160], // y_im
        &proof_bytes[160..192], // y_re
    );
    let c = parse_g1_be(&proof_bytes[192..224], &proof_bytes[224..256]);

    let (Some(a), Some(b), Some(c)) = (a, b, c) else {
        return Ok(Value::Bool(false));
    };
    let proof = ark_groth16::Proof::<Bn254> { a, b, c };

    // VK: for PoC, accept arkworks compressed VK bytes
    let vk = ark_groth16::VerifyingKey::<Bn254>::deserialize_compressed(&*vk_bytes).ok();
    let Some(vk) = vk else {
        return Ok(Value::Bool(false));
    };
    let pvk = ark_groth16::prepare_verifying_key(&vk);

    // public inputs: concatenated 32‑byte big‑endian Fr
    if inputs_bytes.len() % 32 != 0 {
        return Ok(Value::Bool(false));
    }
    let mut inputs = Vec::with_capacity(inputs_bytes.len() / 32);
    for chunk in inputs_bytes.chunks(32) {
        let Some(fr) = parse_fr_be(chunk) else {
            return Ok(Value::Bool(false));
        };
        inputs.push(fr);
    }

    let prepared_inputs = match Groth16::<Bn254>::prepare_inputs(&pvk, &inputs) {
        Ok(v) => v,
        Err(_) => return Ok(Value::Bool(false)),
    };

    let ok = Groth16::<Bn254>::verify_proof_with_prepared_inputs(&pvk, &proof, &prepared_inputs)
        .unwrap_or(false);

    Ok(Value::Bool(ok))
}

pub fn special_bn254_g1_add(
    args: &[SymbolicExpression],
    env: &mut Environment,
    context: &LocalContext,
) -> Result<Value, VmExecutionError> {
    check_argument_count(2, args)?;
    runtime_cost(ClarityCostFunction::Bn254G1Add, env, 0)?;

    let p1_bytes = expect_buffer(eval(&args[0], env, context)?, TypeSignature::BUFFER_64)?;
    let p2_bytes = expect_buffer(eval(&args[1], env, context)?, TypeSignature::BUFFER_64)?;

    if p1_bytes.len() != 64 || p2_bytes.len() != 64 {
        return Err(CheckErrorKind::TypeValueError(
            Box::new(TypeSignature::BUFFER_64),
            Box::new(Value::Sequence(SequenceData::Buffer(BuffData {
                data: p1_bytes,
            }))),
        )
        .into());
    }

    let p1 = parse_g1_be(&p1_bytes[0..32], &p1_bytes[32..64]);
    let p2 = parse_g1_be(&p2_bytes[0..32], &p2_bytes[32..64]);

    let (Some(p1), Some(p2)) = (p1, p2) else {
        return Ok(Value::err_uint(1));
    };

    let sum = (p1.into_group() + p2.into_group()).into_affine();
    let out = Value::buff_from(g1_to_bytes(&sum))
        .map_err(|_| VmInternalError::Expect("Failed to construct buff".into()))?;
    Ok(Value::okay(out).map_err(|_| VmInternalError::Expect("Failed to construct ok".into()))?)
}

pub fn special_bn254_g1_mul(
    args: &[SymbolicExpression],
    env: &mut Environment,
    context: &LocalContext,
) -> Result<Value, VmExecutionError> {
    check_argument_count(2, args)?;
    runtime_cost(ClarityCostFunction::Bn254G1Mul, env, 0)?;

    let p_bytes = expect_buffer(eval(&args[0], env, context)?, TypeSignature::BUFFER_64)?;
    let s_bytes = expect_buffer(eval(&args[1], env, context)?, TypeSignature::BUFFER_32)?;

    if p_bytes.len() != 64 || s_bytes.len() != 32 {
        return Ok(Value::err_uint(1));
    }

    let p = parse_g1_be(&p_bytes[0..32], &p_bytes[32..64]);
    let Some(p) = p else {
        return Ok(Value::err_uint(1));
    };

    let scalar = Fr::from_be_bytes_mod_order(&s_bytes);
    let prod = p
        .into_group()
        .mul_bigint(scalar.into_bigint())
        .into_affine();

    let out = Value::buff_from(g1_to_bytes(&prod))
        .map_err(|_| VmInternalError::Expect("Failed to construct buff".into()))?;
    Ok(Value::okay(out).map_err(|_| VmInternalError::Expect("Failed to construct ok".into()))?)
}

pub fn special_bn254_pairing_check(
    args: &[SymbolicExpression],
    env: &mut Environment,
    context: &LocalContext,
) -> Result<Value, VmExecutionError> {
    check_argument_count(1, args)?;
    runtime_cost(ClarityCostFunction::Bn254PairingCheck, env, 0)?;

    let input = expect_buffer(eval(&args[0], env, context)?, TypeSignature::BUFFER_MAX)?;
    if input.len() % 192 != 0 {
        return Ok(Value::Bool(false));
    }

    let mut g1s = Vec::new();
    let mut g2s = Vec::new();

    for chunk in input.chunks(192) {
        let g1 = parse_g1_be(&chunk[0..32], &chunk[32..64]);
        let g2 = parse_g2_be(
            &chunk[64..96],   // x_im
            &chunk[96..128],  // x_re
            &chunk[128..160], // y_im
            &chunk[160..192], // y_re
        );
        let (Some(g1), Some(g2)) = (g1, g2) else {
            return Ok(Value::Bool(false));
        };
        g1s.push(g1);
        g2s.push(g2);
    }

    let f = Bn254::multi_miller_loop(g1s, g2s);
    let ok = match Bn254::final_exponentiation(f) {
        Some(t) => t.0 == <Bn254 as Pairing>::TargetField::one(),
        None => false,
    };

    Ok(Value::Bool(ok))
}

pub fn special_bn254_g1_neg(
    args: &[SymbolicExpression],
    env: &mut Environment,
    context: &LocalContext,
) -> Result<Value, VmExecutionError> {
    check_argument_count(1, args)?;
    runtime_cost(ClarityCostFunction::Bn254G1Neg, env, 0)?;

    let p_bytes = expect_buffer(eval(&args[0], env, context)?, TypeSignature::BUFFER_64)?;
    if p_bytes.len() != 64 {
        return Ok(Value::err_uint(1));
    }

    let p = parse_g1_be(&p_bytes[0..32], &p_bytes[32..64]);
    let Some(p) = p else {
        return Ok(Value::err_uint(1));
    };

    let neg = (-p.into_group()).into_affine();
    let out = Value::buff_from(g1_to_bytes(&neg))
        .map_err(|_| VmInternalError::Expect("Failed to construct buff".into()))?;
    Ok(Value::okay(out).map_err(|_| VmInternalError::Expect("Failed to construct ok".into()))?)
}

pub fn special_bn254_g2_add(
    args: &[SymbolicExpression],
    env: &mut Environment,
    context: &LocalContext,
) -> Result<Value, VmExecutionError> {
    check_argument_count(2, args)?;
    runtime_cost(ClarityCostFunction::Bn254G2Add, env, 0)?;

    let p1_bytes = expect_buffer(eval(&args[0], env, context)?, TypeSignature::BUFFER_128)?;
    let p2_bytes = expect_buffer(eval(&args[1], env, context)?, TypeSignature::BUFFER_128)?;

    if p1_bytes.len() != 128 || p2_bytes.len() != 128 {
        return Ok(Value::err_uint(1));
    }

    let p1 = parse_g2_be(
        &p1_bytes[0..32],
        &p1_bytes[32..64],
        &p1_bytes[64..96],
        &p1_bytes[96..128],
    );
    let p2 = parse_g2_be(
        &p2_bytes[0..32],
        &p2_bytes[32..64],
        &p2_bytes[64..96],
        &p2_bytes[96..128],
    );

    let (Some(p1), Some(p2)) = (p1, p2) else {
        return Ok(Value::err_uint(1));
    };

    let sum = (p1.into_group() + p2.into_group()).into_affine();
    let out = Value::buff_from(g2_to_bytes(&sum))
        .map_err(|_| VmInternalError::Expect("Failed to construct buff".into()))?;
    Ok(Value::okay(out).map_err(|_| VmInternalError::Expect("Failed to construct ok".into()))?)
}

pub fn special_bn254_g2_mul(
    args: &[SymbolicExpression],
    env: &mut Environment,
    context: &LocalContext,
) -> Result<Value, VmExecutionError> {
    check_argument_count(2, args)?;
    runtime_cost(ClarityCostFunction::Bn254G2Mul, env, 0)?;

    let p_bytes = expect_buffer(eval(&args[0], env, context)?, TypeSignature::BUFFER_128)?;
    let s_bytes = expect_buffer(eval(&args[1], env, context)?, TypeSignature::BUFFER_32)?;

    if p_bytes.len() != 128 || s_bytes.len() != 32 {
        return Ok(Value::err_uint(1));
    }

    let p = parse_g2_be(
        &p_bytes[0..32],
        &p_bytes[32..64],
        &p_bytes[64..96],
        &p_bytes[96..128],
    );
    let Some(p) = p else {
        return Ok(Value::err_uint(1));
    };

    let scalar = Fr::from_be_bytes_mod_order(&s_bytes);
    let prod = p
        .into_group()
        .mul_bigint(scalar.into_bigint())
        .into_affine();

    let out = Value::buff_from(g2_to_bytes(&prod))
        .map_err(|_| VmInternalError::Expect("Failed to construct buff".into()))?;
    Ok(Value::okay(out).map_err(|_| VmInternalError::Expect("Failed to construct ok".into()))?)
}

pub fn special_bn254_g2_neg(
    args: &[SymbolicExpression],
    env: &mut Environment,
    context: &LocalContext,
) -> Result<Value, VmExecutionError> {
    check_argument_count(1, args)?;
    runtime_cost(ClarityCostFunction::Bn254G2Neg, env, 0)?;

    let p_bytes = expect_buffer(eval(&args[0], env, context)?, TypeSignature::BUFFER_128)?;
    if p_bytes.len() != 128 {
        return Ok(Value::err_uint(1));
    }

    let p = parse_g2_be(
        &p_bytes[0..32],
        &p_bytes[32..64],
        &p_bytes[64..96],
        &p_bytes[96..128],
    );
    let Some(p) = p else {
        return Ok(Value::err_uint(1));
    };

    let neg = (-p.into_group()).into_affine();
    let out = Value::buff_from(g2_to_bytes(&neg))
        .map_err(|_| VmInternalError::Expect("Failed to construct buff".into()))?;
    Ok(Value::okay(out).map_err(|_| VmInternalError::Expect("Failed to construct ok".into()))?)
}

pub fn special_bn254_g1_msm(
    args: &[SymbolicExpression],
    env: &mut Environment,
    context: &LocalContext,
) -> Result<Value, VmExecutionError> {
    check_argument_count(1, args)?;

    let input = expect_buffer(eval(&args[0], env, context)?, TypeSignature::BUFFER_MAX)?;
    if input.len() % 96 != 0 {
        return Ok(Value::err_uint(1));
    }
    let terms = (input.len() / 96) as u64;
    runtime_cost(ClarityCostFunction::Bn254G1Msm, env, terms)?;

    let mut acc = G1Affine::identity().into_group();
    for chunk in input.chunks(96) {
        let p = parse_g1_be(&chunk[0..32], &chunk[32..64]);
        let Some(p) = p else {
            return Ok(Value::err_uint(1));
        };
        let scalar = Fr::from_be_bytes_mod_order(&chunk[64..96]);
        acc += p.into_group().mul_bigint(scalar.into_bigint());
    }

    let out = Value::buff_from(g1_to_bytes(&acc.into_affine()))
        .map_err(|_| VmInternalError::Expect("Failed to construct buff".into()))?;
    Ok(Value::okay(out).map_err(|_| VmInternalError::Expect("Failed to construct ok".into()))?)
}
