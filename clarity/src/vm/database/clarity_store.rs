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

#[cfg(feature = "rusqlite")]
use rusqlite::Connection;
use stacks_common::types::StacksEpochId;
use stacks_common::types::chainstate::{StacksBlockId, TrieHash};
use stacks_common::util::hash::{Sha512Trunc256Sum, hex_bytes, to_hex};

use crate::vm::analysis::AnalysisDatabase;
use crate::vm::contexts::GlobalContext;
use crate::vm::database::{
    ClarityDatabase, ClarityDeserializable, ClaritySerializable, NULL_BURN_STATE_DB, NULL_HEADER_DB,
};
use crate::vm::errors::{VmExecutionError, VmInternalError};
use crate::vm::types::codec::packed::SharedPackedValue;
use crate::vm::types::{PrincipalData, QualifiedContractIdentifier, TypeSignature};
use crate::vm::{Value, ValueRef};

/// Canonical and typed representations retained until a Clarity write commits.
///
/// Construction derives every field from one sanitized [`Value`]. Keeping the fields private
/// prevents a backing-store caller from pairing packed bytes with an unrelated canonical hash or
/// logical consensus length.
#[derive(Debug)]
pub struct TypedValueData {
    /// Exact lowercase canonical string used to derive the MARF value hash.
    canonical: String,
    /// Sanitized runtime value prepared by the VM storage operation.
    value: Option<Value>,
    /// Retained composite write, when the caller supplied borrowed payloads.
    shared: Option<SharedPackedValue>,
    /// Consensus bytes already produced for a shared write.
    consensus: Option<Vec<u8>>,
    /// Length of the value's exact consensus serialization.
    consensus_byte_len: u32,
}

impl TypedValueData {
    /// Serialize one VM-prepared value exactly once for deferred physical storage.
    pub fn prepare(value: Value) -> Result<Self, VmInternalError> {
        let _encode = stacks_profiler::diagnostic_span!("Value: Prepare write serialization");
        let consensus = value
            .serialize_to_vec()
            .map_err(|_| VmInternalError::Expect("IOError filling byte buffer.".into()))?;
        stacks_profiler::diagnostics::count("consensus_encoded_bytes", consensus.len() as u64);
        let consensus_byte_len = u32::try_from(consensus.len())
            .map_err(|_| VmInternalError::Expect("Clarity value exceeds u32 length".into()))?;
        Ok(Self {
            canonical: to_hex(&consensus),
            value: Some(value),
            shared: None,
            consensus: None,
            consensus_byte_len,
        })
    }

    /// Prepare a borrowed composite without constructing an intermediate owned value tree.
    pub fn prepare_shared(value: SharedPackedValue) -> Result<Self, VmInternalError> {
        let _encode = stacks_profiler::diagnostic_span!("Value: Prepare write serialization");
        let consensus = value
            .serialize_to_vec()
            .map_err(|_| VmInternalError::Expect("IOError filling byte buffer.".into()))?;
        stacks_profiler::diagnostics::count("consensus_encoded_bytes", consensus.len() as u64);
        let consensus_byte_len = u32::try_from(consensus.len())
            .map_err(|_| VmInternalError::Expect("Clarity value exceeds u32 length".into()))?;
        Ok(Self {
            canonical: to_hex(&consensus),
            value: None,
            shared: Some(value),
            consensus: Some(consensus),
            consensus_byte_len,
        })
    }

    /// Exact bytes for direct transcoding of a shared write.
    pub fn shared_consensus(&self) -> Option<&[u8]> {
        self.consensus.as_deref()
    }

    /// Borrow the exact canonical string associated with the prepared value.
    pub fn canonical(&self) -> &str {
        &self.canonical
    }

    /// Borrow an already-owned value without materializing a retained shared write.
    pub fn owned_value(&self) -> Option<&Value> {
        self.value.as_ref()
    }

    /// Borrow the retained shared write without materializing its value tree.
    pub fn shared_value(&self) -> Option<&SharedPackedValue> {
        self.shared.as_ref()
    }

    /// Borrow the prepared runtime value.
    pub fn value(&self) -> &Value {
        match (&self.value, &self.shared) {
            (Some(value), _) => value,
            (_, Some(value)) => value.materialized_infallible(),
            _ => unreachable!("prepared write representation"),
        }
    }

    /// Return the exact consensus byte length associated with the admitted value.
    pub fn consensus_byte_len(&self) -> u32 {
        self.consensus_byte_len
    }

    /// Consume this typed write and return its canonical text for an untyped backing store.
    pub fn into_canonical(self) -> String {
        self.canonical
    }
}

/// Physical input retained for one logical backing-store edit.
#[derive(Debug)]
pub enum DataStoreValue {
    /// Canonical text for storage backends without a typed physical representation.
    Canonical(String),
    /// A canonical string bound to its prepared typed representation.
    Typed(TypedValueData),
}

impl DataStoreValue {
    /// Borrow the canonical string used for MARF hashing and pending reads.
    pub fn canonical(&self) -> &str {
        match self {
            Self::Canonical(canonical) => canonical,
            Self::Typed(typed) => typed.canonical(),
        }
    }

    /// Consume this value and return its canonical string.
    pub fn into_canonical(self) -> String {
        match self {
            Self::Canonical(canonical) => canonical,
            Self::Typed(typed) => typed.into_canonical(),
        }
    }
}

/// One logical key/value edit committed by a backing store.
#[derive(Debug)]
pub struct DataStoreEntry {
    /// Logical MARF key.
    pub key: String,
    /// Canonical text and optional typed physical-encoding input.
    pub value: DataStoreValue,
}

/// A typed value returned directly by a backing store.
#[derive(Debug)]
pub struct TypedValueResult {
    /// Materialized runtime value.
    pub value: Value,
    /// Length of the equivalent consensus serialization.
    pub serialized_byte_len: u64,
}

/// Materialized or shared-packed representation returned by a typed store read.
#[derive(Debug)]
pub enum StoredValue {
    /// Existing owned runtime representation.
    Owned(Value),
    /// Shared packed record decoded lazily by VM consumers.
    Packed(SharedPackedValue),
}

/// A typed read that can preserve a shared packed record beyond SQLite's row lifetime.
#[derive(Debug)]
pub struct StoredValueResult {
    /// Materialized or shared-packed runtime representation.
    pub value: StoredValue,
    /// Length of the equivalent consensus serialization.
    pub serialized_byte_len: u64,
}

pub struct NullBackingStore {}

pub type SpecialCaseHandler = &'static dyn Fn(
    // the current Clarity global context
    &mut GlobalContext,
    // the current sender
    Option<&PrincipalData>,
    // the current sponsor
    Option<&PrincipalData>,
    // the invoked contract
    &QualifiedContractIdentifier,
    // the invoked function name
    &str,
    // the function parameters
    &[ValueRef<'_>],
    // the result of the function call
    &ValueRef<'_>,
) -> Result<(), VmExecutionError>;

// These functions generally _do not_ return errors, rather, any errors in the underlying storage
//    will _panic_. The rationale for this is that under no condition should the interpreter
//    attempt to continue processing in the event of an unexpected storage error.
pub trait ClarityBackingStore {
    /// Whether writes should retain typed values for an alternate physical representation.
    fn stores_typed_values(&self) -> bool {
        false
    }

    /// put K-V data into the committed datastore
    fn put_all_data(&mut self, items: Vec<(String, String)>) -> Result<(), VmExecutionError>;
    /// Commit logical edits while retaining typed values for physical encodings that need them.
    fn put_all_data_entries(
        &mut self,
        entries: Vec<DataStoreEntry>,
    ) -> Result<(), VmExecutionError> {
        self.put_all_data(
            entries
                .into_iter()
                .map(|entry| (entry.key, entry.value.into_canonical()))
                .collect(),
        )
    }
    /// fetch K-V out of the committed datastore
    fn get_data(&mut self, key: &str) -> Result<Option<String>, VmExecutionError>;
    /// fetch Hash(K)-V out of the commmitted datastore
    fn get_data_from_path(&mut self, hash: &TrieHash) -> Result<Option<String>, VmExecutionError>;
    /// Fetch and decode a declared Clarity value.
    ///
    /// Stores with a typed physical representation override this method. The default preserves the
    /// legacy canonical-string behavior.
    fn get_typed_value(
        &mut self,
        key: &str,
        expected: &TypeSignature,
        epoch: &StacksEpochId,
    ) -> Result<Option<TypedValueResult>, VmExecutionError> {
        let Some(canonical) = self.get_data(key)? else {
            return Ok(None);
        };
        let value = Value::try_deserialize_hex_at_epoch(&canonical, expected, epoch)
            .map_err(|error| VmInternalError::DBError(error.to_string()))?;
        Ok(Some(TypedValueResult {
            value,
            serialized_byte_len: canonical.len() as u64 / 2,
        }))
    }

    /// Fetch a declared value while preserving an alternate shared representation when possible.
    ///
    /// The default adapts legacy and test stores through the existing owned typed read.
    fn get_stored_value(
        &mut self,
        key: &str,
        expected: &TypeSignature,
        epoch: &StacksEpochId,
    ) -> Result<Option<StoredValueResult>, VmExecutionError> {
        self.get_typed_value(key, expected, epoch).map(|result| {
            result.map(|result| StoredValueResult {
                value: StoredValue::Owned(result.value),
                serialized_byte_len: result.serialized_byte_len,
            })
        })
    }
    /// fetch K-V out of the committed datastore, along with the byte representation
    ///  of the Merkle proof for that key-value pair
    fn get_data_with_proof(
        &mut self,
        key: &str,
    ) -> Result<Option<(String, Vec<u8>)>, VmExecutionError>;
    fn get_data_with_proof_from_path(
        &mut self,
        hash: &TrieHash,
    ) -> Result<Option<(String, Vec<u8>)>, VmExecutionError>;
    fn has_entry(&mut self, key: &str) -> Result<bool, VmExecutionError> {
        Ok(self.get_data(key)?.is_some())
    }

    /// change the current MARF context to service reads from a different chain_tip
    ///   used to implement time-shifted evaluation.
    /// returns the previous block header hash on success
    fn set_block_hash(&mut self, bhh: StacksBlockId) -> Result<StacksBlockId, VmExecutionError>;

    /// Is None if `block_height` >= the "currently" under construction Stacks block height.
    fn get_block_at_height(&mut self, height: u32) -> Option<StacksBlockId>;

    /// this function returns the current block height, as viewed by this marfed-kv structure,
    ///  i.e., it changes on time-shifted evaluation. the open_chain_tip functions always
    ///   return data about the chain tip that is currently open for writing.
    fn get_current_block_height(&mut self) -> u32;

    fn get_open_chain_tip_height(&mut self) -> u32;
    fn get_open_chain_tip(&mut self) -> StacksBlockId;

    #[cfg(feature = "rusqlite")]
    fn get_side_store(&mut self) -> &Connection;

    fn get_cc_special_cases_handler(&self) -> Option<SpecialCaseHandler> {
        None
    }

    /// The contract commitment is the hash of the contract, plus the block height in
    ///   which the contract was initialized.
    fn make_contract_commitment(&mut self, contract_hash: Sha512Trunc256Sum) -> String {
        let block_height = self.get_open_chain_tip_height();
        let cc = ContractCommitment {
            hash: contract_hash,
            block_height,
        };
        cc.serialize()
    }

    /// This function is used to obtain a committed contract hash, and the block header hash of the block
    ///   in which the contract was initialized. This data is used to store contract metadata in the side
    ///   store.
    fn get_contract_hash(
        &mut self,
        contract: &QualifiedContractIdentifier,
    ) -> Result<(StacksBlockId, Sha512Trunc256Sum), VmExecutionError>;

    fn insert_metadata(
        &mut self,
        contract: &QualifiedContractIdentifier,
        key: &str,
        value: &str,
    ) -> Result<(), VmExecutionError>;

    fn get_metadata(
        &mut self,
        contract: &QualifiedContractIdentifier,
        key: &str,
    ) -> Result<Option<String>, VmExecutionError>;

    fn get_metadata_manual(
        &mut self,
        at_height: u32,
        contract: &QualifiedContractIdentifier,
        key: &str,
    ) -> Result<Option<String>, VmExecutionError>;

    fn put_all_metadata(
        &mut self,
        items: Vec<((QualifiedContractIdentifier, String), String)>,
    ) -> Result<(), VmExecutionError> {
        for ((contract, key), value) in items.into_iter() {
            self.insert_metadata(&contract, &key, &value)?;
        }
        Ok(())
    }
}

// TODO: Figure out where this belongs
pub fn make_contract_hash_key(contract: &QualifiedContractIdentifier) -> String {
    format!("clarity-contract::{contract}")
}

pub struct ContractCommitment {
    pub hash: Sha512Trunc256Sum,
    pub block_height: u32,
}

impl ClaritySerializable for ContractCommitment {
    fn serialize(&self) -> String {
        format!("{}{}", self.hash, to_hex(&self.block_height.to_be_bytes()))
    }
}

impl ClarityDeserializable<ContractCommitment> for ContractCommitment {
    fn deserialize(input: &str) -> Result<ContractCommitment, VmExecutionError> {
        if input.len() != 72 {
            return Err(VmInternalError::Expect("Unexpected input length".into()).into());
        }
        let hash = Sha512Trunc256Sum::from_hex(&input[0..64])
            .map_err(|_| VmInternalError::Expect("Hex decode fail.".into()))?;
        let height_bytes = hex_bytes(&input[64..72])
            .map_err(|_| VmInternalError::Expect("Hex decode fail.".into()))?;
        let block_height = u32::from_be_bytes(
            height_bytes
                .as_slice()
                .try_into()
                .map_err(|_| VmInternalError::Expect("Block height decode fail.".into()))?,
        );
        Ok(ContractCommitment { hash, block_height })
    }
}

impl Default for NullBackingStore {
    fn default() -> Self {
        NullBackingStore::new()
    }
}

impl NullBackingStore {
    pub fn new() -> Self {
        NullBackingStore {}
    }

    pub fn as_clarity_db(&mut self) -> ClarityDatabase<'_> {
        ClarityDatabase::new(self, &NULL_HEADER_DB, &NULL_BURN_STATE_DB)
    }

    pub fn as_analysis_db(&mut self) -> AnalysisDatabase<'_> {
        AnalysisDatabase::new(self)
    }
}

#[allow(clippy::panic)]
impl ClarityBackingStore for NullBackingStore {
    fn set_block_hash(&mut self, _bhh: StacksBlockId) -> Result<StacksBlockId, VmExecutionError> {
        panic!("NullBackingStore can't set block hash")
    }

    fn get_data(&mut self, _key: &str) -> Result<Option<String>, VmExecutionError> {
        panic!("NullBackingStore can't retrieve data")
    }

    fn get_data_from_path(&mut self, _hash: &TrieHash) -> Result<Option<String>, VmExecutionError> {
        panic!("NullBackingStore can't retrieve data")
    }

    fn get_data_with_proof(
        &mut self,
        _key: &str,
    ) -> Result<Option<(String, Vec<u8>)>, VmExecutionError> {
        panic!("NullBackingStore can't retrieve data")
    }

    fn get_data_with_proof_from_path(
        &mut self,
        _hash: &TrieHash,
    ) -> Result<Option<(String, Vec<u8>)>, VmExecutionError> {
        panic!("NullBackingStore can't retrieve data")
    }

    #[cfg(feature = "rusqlite")]
    fn get_side_store(&mut self) -> &Connection {
        panic!("NullBackingStore has no side store")
    }

    fn get_block_at_height(&mut self, _height: u32) -> Option<StacksBlockId> {
        panic!("NullBackingStore can't get block at height")
    }

    fn get_open_chain_tip(&mut self) -> StacksBlockId {
        panic!("NullBackingStore can't open chain tip")
    }

    fn get_open_chain_tip_height(&mut self) -> u32 {
        panic!("NullBackingStore can't get open chain tip height")
    }

    fn get_current_block_height(&mut self) -> u32 {
        panic!("NullBackingStore can't get current block height")
    }

    fn put_all_data(&mut self, mut _items: Vec<(String, String)>) -> Result<(), VmExecutionError> {
        panic!("NullBackingStore cannot put")
    }

    fn get_contract_hash(
        &mut self,
        _contract: &QualifiedContractIdentifier,
    ) -> Result<(StacksBlockId, Sha512Trunc256Sum), VmExecutionError> {
        panic!("NullBackingStore cannot get_contract_hash")
    }

    fn insert_metadata(
        &mut self,
        _contract: &QualifiedContractIdentifier,
        _key: &str,
        _value: &str,
    ) -> Result<(), VmExecutionError> {
        panic!("NullBackingStore cannot insert_metadata")
    }

    fn get_metadata(
        &mut self,
        _contract: &QualifiedContractIdentifier,
        _key: &str,
    ) -> Result<Option<String>, VmExecutionError> {
        panic!("NullBackingStore cannot get_metadata")
    }

    fn get_metadata_manual(
        &mut self,
        _at_height: u32,
        _contract: &QualifiedContractIdentifier,
        _key: &str,
    ) -> Result<Option<String>, VmExecutionError> {
        panic!("NullBackingStore cannot get_metadata_manual")
    }
}
