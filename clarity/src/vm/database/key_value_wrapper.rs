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

//! Nested rollback buffering for Clarity data and metadata writes.

use std::collections::HashMap;
use std::hash::Hash;

use stacks_common::types::StacksEpochId;
use stacks_common::types::chainstate::{StacksBlockId, TrieHash};
use stacks_common::util::hash::Sha512Trunc256Sum;

use super::clarity_store::SpecialCaseHandler;
use super::{
    ClarityBackingStore, ClarityDeserializable, DataStoreEntry, DataStoreValue, StoredValue,
    StoredValueResult, TypedValueData, TypedValueResult,
};
use crate::vm::Value;
use crate::vm::database::clarity_store::{ContractCommitment, make_contract_hash_key};
use crate::vm::errors::{VmExecutionError, VmInternalError};
use crate::vm::types::serialization::SerializationError;
use crate::vm::types::{QualifiedContractIdentifier, TypeSignature};

#[cfg(feature = "rollback_value_check")]
type RollbackValueCheck = String;
#[cfg(not(feature = "rollback_value_check"))]
type RollbackValueCheck = ();

#[cfg(not(feature = "rollback_value_check"))]
fn rollback_value_check(_value: &str, _check: &RollbackValueCheck) {}

#[cfg(not(feature = "rollback_value_check"))]
fn rollback_edits_push<T>(edits: &mut Vec<(T, RollbackValueCheck)>, key: T, _value: &str) {
    edits.push((key, ()));
}
// this function is used to check the lookup map when committing at the "bottom" of the
//   wrapper -- i.e., when committing to the underlying store. for the _unchecked_ implementation
//   this is used to get the edit _value_ out of the lookupmap, for used in the subsequent `put_all`
//   command.
#[cfg(not(feature = "rollback_value_check"))]
fn rollback_check_pre_bottom_commit<T>(
    edits: Vec<(T, RollbackValueCheck)>,
    lookup_map: &mut HashMap<T, Vec<String>>,
) -> Result<Vec<(T, String)>, VmInternalError>
where
    T: Eq + Hash + Clone,
{
    for edit_history in lookup_map.values_mut() {
        edit_history.reverse();
    }

    let output = edits
        .into_iter()
        .map(|(key, _)| {
            let value = rollback_lookup_map(&key, &(), lookup_map)?;
            Ok((key, value))
        })
        .collect();

    assert!(lookup_map.is_empty());
    output
}

#[cfg(feature = "rollback_value_check")]
fn rollback_value_check(value: &str, check: &RollbackValueCheck) {
    assert_eq!(value, check)
}
#[cfg(feature = "rollback_value_check")]
fn rollback_edits_push<T>(edits: &mut Vec<(T, RollbackValueCheck)>, key: T, value: &str)
where
    T: Eq + Hash + Clone,
{
    edits.push((key, value.to_owned()));
}
// this function is used to check the lookup map when committing at the "bottom" of the
//   wrapper -- i.e., when committing to the underlying store.
#[cfg(feature = "rollback_value_check")]
fn rollback_check_pre_bottom_commit<T>(
    edits: Vec<(T, RollbackValueCheck)>,
    lookup_map: &mut HashMap<T, Vec<String>>,
) -> Result<Vec<(T, String)>, VmInternalError>
where
    T: Eq + Hash + Clone,
{
    for edit_history in lookup_map.values_mut() {
        edit_history.reverse();
    }
    for (key, value) in edits.iter() {
        let _ = rollback_lookup_map(key, value, lookup_map);
    }
    assert!(lookup_map.is_empty());
    Ok(edits)
}

/// Result structure for fetched values from the
///  underlying store.
#[derive(Debug)]
pub struct ValueResult {
    pub value: Value,
    pub serialized_byte_len: u64,
}

/// One pending data write with an atomic canonical/typed representation.
///
/// Boxing keeps canonical-only entries compact while preventing typed values from being separated
/// from the canonical strings and lengths derived from them.
#[derive(Debug)]
enum PendingDataValue {
    /// Canonical text for a backing store without typed physical encoding.
    Canonical(String),
    /// Canonical and admitted representations for a typed backing store.
    Typed(Box<TypedValueData>),
}

impl PendingDataValue {
    /// Borrow the canonical string used by rollback checks and pending reads.
    fn canonical(&self) -> &str {
        match self {
            Self::Canonical(canonical) => canonical,
            Self::Typed(typed) => typed.canonical(),
        }
    }

    /// Consume this pending value for the backing-store commit boundary.
    fn into_data_store_value(self) -> DataStoreValue {
        match self {
            Self::Canonical(canonical) => DataStoreValue::Canonical(canonical),
            Self::Typed(typed) => DataStoreValue::Typed(*typed),
        }
    }
}

pub struct RollbackContext {
    edits: Vec<(String, RollbackValueCheck)>,
    metadata_edits: Vec<((QualifiedContractIdentifier, String), RollbackValueCheck)>,
}

pub struct RollbackWrapper<'a> {
    // the underlying key-value storage.
    store: &'a mut dyn ClarityBackingStore,
    // lookup_map is a history of edits for a given key.
    //   in order of least-recent to most-recent at the tail.
    //   this allows ~ O(1) lookups, and ~ O(1) commits, roll-backs (amortized by # of PUTs).
    lookup_map: HashMap<String, Vec<PendingDataValue>>,
    metadata_lookup_map: HashMap<(QualifiedContractIdentifier, String), Vec<String>>,
    // stack keeps track of the most recent rollback context, which tells us which
    //   edits were performed by which context. at the moment, each context's edit history
    //   is a separate Vec which must be drained into the parent on commits, meaning that
    //   the amortized cost of committing a value isn't O(1), but actually O(k) where k is
    //   stack depth.
    //  TODO: The solution to this is to just have a _single_ edit stack, and merely store indexes
    //   to indicate a given contexts "start depth".
    stack: Vec<RollbackContext>,
    query_pending_data: bool,
}

// This is used for preserving rollback data longer
//   than a BackingStore pointer. This is useful to prevent
//   a real mess of lifetime parameters in the database/context
//   and eval code.
pub struct RollbackWrapperPersistedLog {
    lookup_map: HashMap<String, Vec<PendingDataValue>>,
    metadata_lookup_map: HashMap<(QualifiedContractIdentifier, String), Vec<String>>,
    stack: Vec<RollbackContext>,
}

impl From<RollbackWrapper<'_>> for RollbackWrapperPersistedLog {
    fn from(o: RollbackWrapper<'_>) -> RollbackWrapperPersistedLog {
        RollbackWrapperPersistedLog {
            lookup_map: o.lookup_map,
            metadata_lookup_map: o.metadata_lookup_map,
            stack: o.stack,
        }
    }
}

impl Default for RollbackWrapperPersistedLog {
    fn default() -> Self {
        Self::new()
    }
}

impl RollbackWrapperPersistedLog {
    pub fn new() -> RollbackWrapperPersistedLog {
        RollbackWrapperPersistedLog {
            lookup_map: HashMap::new(),
            metadata_lookup_map: HashMap::new(),
            stack: Vec::new(),
        }
    }

    pub fn nest(&mut self) {
        self.stack.push(RollbackContext {
            edits: Vec::new(),
            metadata_edits: Vec::new(),
        });
    }
}

fn rollback_lookup_map<T>(
    key: &T,
    value: &RollbackValueCheck,
    lookup_map: &mut HashMap<T, Vec<String>>,
) -> Result<String, VmInternalError>
where
    T: Eq + Hash + Clone,
{
    let popped_value;
    let remove_edit_deque = {
        let key_edit_history = lookup_map.get_mut(key).ok_or_else(|| {
            VmInternalError::Expect(
                "ERROR: Clarity VM had edit log entry, but not lookup_map entry".into(),
            )
        })?;
        popped_value = key_edit_history.pop().ok_or_else(|| {
            VmInternalError::Expect("ERROR: expected value in edit history".into())
        })?;
        rollback_value_check(&popped_value, value);
        key_edit_history.is_empty()
    };
    if remove_edit_deque {
        lookup_map.remove(key);
    }
    Ok(popped_value)
}

/// Pop one pending data write while keeping its canonical and typed forms atomic.
fn rollback_data_lookup_map(
    key: &str,
    value: &RollbackValueCheck,
    lookup_map: &mut HashMap<String, Vec<PendingDataValue>>,
) -> Result<PendingDataValue, VmInternalError> {
    let (popped_value, remove_edit_deque) = {
        let key_edit_history = lookup_map.get_mut(key).ok_or_else(|| {
            VmInternalError::Expect(
                "ERROR: Clarity VM had data edit log entry, but not data lookup entry".into(),
            )
        })?;
        let popped_value = key_edit_history.pop().ok_or_else(|| {
            VmInternalError::Expect("ERROR: expected value in data edit history".into())
        })?;
        rollback_value_check(popped_value.canonical(), value);
        (popped_value, key_edit_history.is_empty())
    };
    if remove_edit_deque {
        lookup_map.remove(key);
    }
    Ok(popped_value)
}

/// Drain rollback histories into the ordered entries committed to the backing store.
fn rollback_data_pre_bottom_commit(
    edits: Vec<(String, RollbackValueCheck)>,
    lookup_map: &mut HashMap<String, Vec<PendingDataValue>>,
) -> Result<Vec<DataStoreEntry>, VmInternalError> {
    for edit_history in lookup_map.values_mut() {
        edit_history.reverse();
    }

    let output = edits
        .into_iter()
        .map(|(key, check)| {
            let pending = rollback_data_lookup_map(&key, &check, lookup_map)?;
            Ok(DataStoreEntry {
                key,
                value: pending.into_data_store_value(),
            })
        })
        .collect::<Result<_, VmInternalError>>()?;

    assert!(lookup_map.is_empty());
    Ok(output)
}

impl<'a> RollbackWrapper<'a> {
    pub fn new(store: &'a mut dyn ClarityBackingStore) -> RollbackWrapper<'a> {
        RollbackWrapper {
            store,
            lookup_map: HashMap::new(),
            metadata_lookup_map: HashMap::new(),
            stack: Vec::new(),
            query_pending_data: true,
        }
    }

    pub fn from_persisted_log(
        store: &'a mut dyn ClarityBackingStore,
        log: RollbackWrapperPersistedLog,
    ) -> RollbackWrapper<'a> {
        RollbackWrapper {
            store,
            lookup_map: log.lookup_map,
            metadata_lookup_map: log.metadata_lookup_map,
            stack: log.stack,
            query_pending_data: true,
        }
    }

    pub fn get_cc_special_cases_handler(&self) -> Option<SpecialCaseHandler> {
        self.store.get_cc_special_cases_handler()
    }

    pub fn nest(&mut self) {
        self.stack.push(RollbackContext {
            edits: Vec::new(),
            metadata_edits: Vec::new(),
        });
    }

    // Rollback the child's edits.
    //   this clears all edits from the child's edit queue,
    //     and removes any of those edits from the lookup map.
    pub fn rollback(&mut self) -> Result<(), VmInternalError> {
        let mut last_item = self.stack.pop().ok_or_else(|| {
            VmInternalError::Expect("ERROR: Clarity VM attempted to commit past the stack.".into())
        })?;

        last_item.edits.reverse();
        last_item.metadata_edits.reverse();

        for (key, value) in last_item.edits.drain(..) {
            rollback_data_lookup_map(&key, &value, &mut self.lookup_map)?;
        }

        for (key, value) in last_item.metadata_edits.drain(..) {
            rollback_lookup_map(&key, &value, &mut self.metadata_lookup_map)?;
        }

        Ok(())
    }

    pub fn depth(&self) -> usize {
        self.stack.len()
    }

    pub fn commit(&mut self) -> Result<(), VmInternalError> {
        let _writeback_diagnostic = stacks_profiler::diagnostic_span!("VM: Rollback commit");
        let stores_typed_values = self.store.stores_typed_values();
        let mut last_item = self.stack.pop().ok_or_else(|| {
            VmInternalError::Expect("ERROR: Clarity VM attempted to commit past the stack.".into())
        })?;

        if let Some(next_up) = self.stack.last_mut() {
            // bubble up to the next item in the stack
            // last_mut() must exist because of the if-statement
            for (key, value) in last_item.edits.drain(..) {
                next_up.edits.push((key, value));
            }
            for (key, value) in last_item.metadata_edits.drain(..) {
                next_up.metadata_edits.push((key, value));
            }
        } else {
            // stack is empty, committing to the backing store
            let all_edits = rollback_data_pre_bottom_commit(last_item.edits, &mut self.lookup_map)?;
            if !all_edits.is_empty() {
                if stores_typed_values {
                    self.store.put_all_data_entries(all_edits).map_err(|e| {
                        VmInternalError::Expect(format!(
                            "ERROR: Failed to commit data to sql store: {e:?}"
                        ))
                    })?;
                } else {
                    let all_edits = all_edits
                        .into_iter()
                        .map(|entry| (entry.key, entry.value.into_canonical()))
                        .collect();
                    self.store.put_all_data(all_edits).map_err(|e| {
                        VmInternalError::Expect(format!(
                            "ERROR: Failed to commit data to sql store: {e:?}"
                        ))
                    })?;
                }
            }

            let metadata_edits = rollback_check_pre_bottom_commit(
                last_item.metadata_edits,
                &mut self.metadata_lookup_map,
            )?;
            if !metadata_edits.is_empty() {
                self.store.put_all_metadata(metadata_edits).map_err(|e| {
                    VmInternalError::Expect(format!(
                        "ERROR: Failed to commit data to sql store: {e:?}"
                    ))
                })?;
            }
        }

        Ok(())
    }
}

/// Append one canonical/typed write pair to the current rollback history.
fn inner_put_data(
    lookup_map: &mut HashMap<String, Vec<PendingDataValue>>,
    edits: &mut Vec<(String, RollbackValueCheck)>,
    key: String,
    value: PendingDataValue,
) {
    let key_edit_deque = lookup_map.entry(key.clone()).or_default();
    rollback_edits_push(edits, key.clone(), value.canonical());
    key_edit_deque.push(value);
}

impl RollbackWrapper<'_> {
    /// Whether the backing store consumes typed write metadata.
    pub fn stores_typed_values(&self) -> bool {
        self.store.stores_typed_values()
    }

    pub fn put_data(&mut self, key: &str, value: &str) -> Result<(), VmExecutionError> {
        let current = self.stack.last_mut().ok_or_else(|| {
            VmInternalError::Expect("ERROR: Clarity VM attempted PUT on non-nested context.".into())
        })?;

        inner_put_data(
            &mut self.lookup_map,
            &mut current.edits,
            key.to_string(),
            PendingDataValue::Canonical(value.to_string()),
        );
        Ok(())
    }

    /// Buffer an admitted Clarity value and its inseparable canonical representation.
    pub fn put_typed_value(
        &mut self,
        key: &str,
        typed: TypedValueData,
    ) -> Result<(), VmExecutionError> {
        let current = self.stack.last_mut().ok_or_else(|| {
            VmInternalError::Expect("ERROR: Clarity VM attempted PUT on non-nested context.".into())
        })?;
        inner_put_data(
            &mut self.lookup_map,
            &mut current.edits,
            key.to_owned(),
            PendingDataValue::Typed(Box::new(typed)),
        );
        Ok(())
    }

    /// Returns whether or not the wrapper is currently retargeted to another block by e.g. an
    /// `at-block` scope.
    pub fn is_retargeted(&self) -> bool {
        !self.query_pending_data
    }

    /// `query_pending_data` indicates whether the rollback wrapper should query the rollback
    ///    wrapper's pending data on reads. This is set to `false` during (at-block ...) closures,
    ///    and `true` otherwise.
    ///
    pub fn set_block_hash(
        &mut self,
        bhh: StacksBlockId,
        query_pending_data: bool,
    ) -> Result<StacksBlockId, VmExecutionError> {
        self.store.set_block_hash(bhh).inspect(|_| {
            // use and_then so that query_pending_data is only set once set_block_hash succeeds
            //  this doesn't matter in practice, because a set_block_hash failure always aborts
            //  the transaction with a runtime error (destroying its environment), but it's much
            //  better practice to do this, especially if the abort behavior changes in the future.
            self.query_pending_data = query_pending_data;
        })
    }

    /// this function will only return commitment proofs for values _already_ materialized
    ///  in the underlying store. otherwise it returns None.
    pub fn get_data_with_proof<T>(
        &mut self,
        key: &str,
    ) -> Result<Option<(T, Vec<u8>)>, VmExecutionError>
    where
        T: ClarityDeserializable<T>,
    {
        self.store
            .get_data_with_proof(key)?
            .map(|(value, proof)| Ok((T::deserialize(&value)?, proof)))
            .transpose()
    }

    /// this function will only return commitment proofs for values _already_ materialized
    ///  in the underlying store. otherwise it returns None.
    pub fn get_data_with_proof_by_hash<T>(
        &mut self,
        hash: &TrieHash,
    ) -> Result<Option<(T, Vec<u8>)>, VmExecutionError>
    where
        T: ClarityDeserializable<T>,
    {
        self.store
            .get_data_with_proof_from_path(hash)?
            .map(|(value, proof)| Ok((T::deserialize(&value)?, proof)))
            .transpose()
    }

    pub fn get_data<T>(&mut self, key: &str) -> Result<Option<T>, VmExecutionError>
    where
        T: ClarityDeserializable<T>,
    {
        self.stack.last().ok_or_else(|| {
            VmInternalError::Expect("ERROR: Clarity VM attempted GET on non-nested context.".into())
        })?;

        if self.query_pending_data
            && let Some(pending_value) = self.lookup_map.get(key).and_then(|x| x.last())
        {
            // if there's pending data and we're querying pending data, return here
            return Some(T::deserialize(pending_value.canonical())).transpose();
        }
        // otherwise, lookup from store
        self.store
            .get_data(key)?
            .map(|x| T::deserialize(&x))
            .transpose()
    }

    /// DO NOT USE IN CONSENSUS CODE.
    ///
    /// Load data directly from the underlying store, given its trie hash.  The lookup map will not
    /// be used.
    ///
    /// This should never be called from within the Clarity VM, or via block-processing.  It's only
    /// meant to be used by the RPC system.
    pub fn get_data_by_hash<T>(&mut self, hash: &TrieHash) -> Result<Option<T>, VmExecutionError>
    where
        T: ClarityDeserializable<T>,
    {
        self.store
            .get_data_from_path(hash)?
            .map(|x| T::deserialize(&x))
            .transpose()
    }

    pub fn deserialize_value(
        value_hex: &str,
        expected: &TypeSignature,
        epoch: &StacksEpochId,
    ) -> Result<ValueResult, SerializationError> {
        let _decode = stacks_profiler::diagnostic_span!("Value: Canonical decode");
        stacks_profiler::diagnostics::count("canonical_decodes", 1);
        stacks_profiler::diagnostics::count("canonical_decode_bytes", value_hex.len() as u64 / 2);
        let serialized_byte_len = value_hex.len() as u64 / 2;
        let value = Value::try_deserialize_hex_at_epoch(value_hex, expected, epoch)?;

        Ok(ValueResult {
            value,
            serialized_byte_len,
        })
    }

    /// Get a Clarity value from the underlying Clarity KV store.
    /// Returns Some if found, with the Clarity Value and the serialized byte length of the value.
    pub fn get_value(
        &mut self,
        key: &str,
        expected: &TypeSignature,
        epoch: &StacksEpochId,
    ) -> Result<Option<ValueResult>, SerializationError> {
        self.stack.last().ok_or_else(|| {
            SerializationError::DeserializationFailure(
                "ERROR: Clarity VM attempted GET on non-nested context.".into(),
            )
        })?;

        if self.query_pending_data
            && let Some(x) = self.lookup_map.get(key).and_then(|x| x.last())
        {
            // Reapply the requested schema so declared bounds and callable metadata are restored
            // exactly as they are for a committed read.
            return Ok(Some(Self::deserialize_value(
                x.canonical(),
                expected,
                epoch,
            )?));
        }
        let stored_data = self
            .store
            .get_typed_value(key, expected, epoch)
            .map_err(|error| {
                SerializationError::DeserializationFailure(format!(
                    "ERROR: Clarity backing store failure for key {key}: {error}"
                ))
            })?;
        Ok(stored_data.map(
            |TypedValueResult {
                 value,
                 serialized_byte_len,
             }| ValueResult {
                value,
                serialized_byte_len,
            },
        ))
    }

    /// Get a Clarity value while preserving shared packed storage when possible.
    pub fn get_stored_value(
        &mut self,
        key: &str,
        expected: &TypeSignature,
        epoch: &StacksEpochId,
    ) -> Result<Option<StoredValueResult>, SerializationError> {
        self.stack.last().ok_or_else(|| {
            SerializationError::DeserializationFailure(
                "ERROR: Clarity VM attempted GET on non-nested context.".into(),
            )
        })?;

        if self.query_pending_data
            && let Some(value) = self.lookup_map.get(key).and_then(|values| values.last())
        {
            let value = Self::deserialize_value(value.canonical(), expected, epoch)?;
            return Ok(Some(StoredValueResult {
                value: StoredValue::Owned(value.value),
                serialized_byte_len: value.serialized_byte_len,
            }));
        }
        self.store
            .get_stored_value(key, expected, epoch)
            .map_err(|error| {
                SerializationError::DeserializationFailure(format!(
                    "ERROR: Clarity backing store failure for key {key}: {error}"
                ))
            })
    }

    /// This is the height we are currently constructing. It comes from the MARF.
    pub fn get_current_block_height(&mut self) -> u32 {
        self.store.get_current_block_height()
    }

    /// Is None if `block_height` >= the "currently" under construction Stacks block height.
    pub fn get_block_header_hash(&mut self, block_height: u32) -> Option<StacksBlockId> {
        self.store.get_block_at_height(block_height)
    }

    pub fn get_contract_hash(
        &mut self,
        contract: &QualifiedContractIdentifier,
    ) -> Result<Option<Sha512Trunc256Sum>, VmExecutionError> {
        let key = make_contract_hash_key(contract);
        let s = match self.get_data::<String>(&key)? {
            Some(s) => s,
            None => return Ok(None),
        };
        let cc = ContractCommitment::deserialize(&s)?;
        Ok(Some(cc.hash))
    }

    pub fn prepare_for_contract_metadata(
        &mut self,
        contract: &QualifiedContractIdentifier,
        content_hash: Sha512Trunc256Sum,
    ) -> Result<(), VmExecutionError> {
        let key = make_contract_hash_key(contract);
        let value = self.store.make_contract_commitment(content_hash);
        self.put_data(&key, &value)
    }

    pub fn insert_metadata(
        &mut self,
        contract: &QualifiedContractIdentifier,
        key: &str,
        value: &str,
    ) -> Result<(), VmInternalError> {
        let current = self.stack.last_mut().ok_or_else(|| {
            VmInternalError::Expect("ERROR: Clarity VM attempted PUT on non-nested context.".into())
        })?;

        let metadata_key = (contract.clone(), key.to_string());
        let edit_deque = self
            .metadata_lookup_map
            .entry(metadata_key.clone())
            .or_default();
        rollback_edits_push(&mut current.metadata_edits, metadata_key, value);
        edit_deque.push(value.to_owned());
        Ok(())
    }

    // Throws a NoSuchContract error if contract doesn't exist,
    //   returns None if there is no such metadata field.
    pub fn get_metadata(
        &mut self,
        contract: &QualifiedContractIdentifier,
        key: &str,
    ) -> Result<Option<String>, VmExecutionError> {
        self.stack.last().ok_or_else(|| {
            VmInternalError::Expect("ERROR: Clarity VM attempted GET on non-nested context.".into())
        })?;

        // This is THEORETICALLY a spurious clone, but it's hard to turn something like
        //  (&A, &B) into &(A, B).
        let metadata_key = (contract.clone(), key.to_string());
        let lookup_result = if self.query_pending_data {
            self.metadata_lookup_map
                .get(&metadata_key)
                .and_then(|x| x.last().cloned())
        } else {
            None
        };

        match lookup_result {
            Some(x) => Ok(Some(x)),
            None => self.store.get_metadata(contract, key),
        }
    }

    // Throws a NoSuchContract error if contract doesn't exist,
    //   returns None if there is no such metadata field.
    pub fn get_metadata_manual(
        &mut self,
        at_height: u32,
        contract: &QualifiedContractIdentifier,
        key: &str,
    ) -> Result<Option<String>, VmExecutionError> {
        self.stack.last().ok_or_else(|| {
            VmInternalError::Expect("ERROR: Clarity VM attempted GET on non-nested context.".into())
        })?;

        // This is THEORETICALLY a spurious clone, but it's hard to turn something like
        //  (&A, &B) into &(A, B).
        let metadata_key = (contract.clone(), key.to_string());
        let lookup_result = if self.query_pending_data {
            self.metadata_lookup_map
                .get(&metadata_key)
                .and_then(|x| x.last().cloned())
        } else {
            None
        };

        match lookup_result {
            Some(x) => Ok(Some(x)),
            None => self.store.get_metadata_manual(at_height, contract, key),
        }
    }

    pub fn has_entry(&mut self, key: &str) -> Result<bool, VmExecutionError> {
        self.stack.last().ok_or_else(|| {
            VmInternalError::Expect("ERROR: Clarity VM attempted GET on non-nested context.".into())
        })?;
        if self.query_pending_data && self.lookup_map.contains_key(key) {
            Ok(true)
        } else {
            self.store.has_entry(key)
        }
    }

    pub fn has_metadata_entry(
        &mut self,
        contract: &QualifiedContractIdentifier,
        key: &str,
    ) -> bool {
        matches!(self.get_metadata(contract, key), Ok(Some(_)))
    }

    /// Returns `true` if any of the given metadata keys for `contract` has an uncommitted edit in
    /// the rollback stack (i.e. would be served from pending data rather than the backing store on
    /// a `get_metadata` call).
    ///
    /// Used by caching implementations to avoid caching reads whose metadata could later be rolled
    /// back.
    pub fn has_pending_metadata(
        &self,
        contract: &QualifiedContractIdentifier,
        keys: &[&str],
    ) -> bool {
        // Retargeted wrappers always read from the backing store, so pending metadata is
        // irrelevant.
        if self.is_retargeted() {
            return false;
        }

        keys.iter().any(|key| {
            let metadata_key = (contract.clone(), (*key).to_string());
            self.metadata_lookup_map.contains_key(&metadata_key)
        })
    }
}

#[cfg(test)]
mod typed_write_tests {
    use std::assert_matches;

    use super::*;

    fn typed_uint(value: u128) -> TypedValueData {
        TypedValueData::prepare(Value::UInt(value)).unwrap()
    }

    #[test]
    fn prepared_typed_value_binds_canonical_and_length() {
        let typed = typed_uint(42);
        let consensus = typed.value().serialize_to_vec().unwrap();
        assert_eq!(
            typed.canonical(),
            stacks_common::util::hash::to_hex(&consensus)
        );
        assert_eq!(typed.consensus_byte_len(), consensus.len() as u32);
    }

    #[test]
    fn bottom_commit_keeps_typed_metadata_aligned_with_repeated_keys() {
        let key = "vm::contract::0::entry".to_owned();
        let typed = typed_uint(1);
        let typed_canonical = typed.canonical().to_owned();
        let mut lookup_map = HashMap::from([(
            key.clone(),
            vec![
                PendingDataValue::Typed(Box::new(typed)),
                PendingDataValue::Canonical("03".into()),
            ],
        )]);
        let mut edits = Vec::new();
        rollback_edits_push(&mut edits, key.clone(), &typed_canonical);
        rollback_edits_push(&mut edits, key, "03");

        let entries = rollback_data_pre_bottom_commit(edits, &mut lookup_map).unwrap();
        assert_eq!(entries.len(), 2);
        assert_matches!(
            &entries[0].value,
            DataStoreValue::Typed(typed) if typed.value() == &Value::UInt(1)
        );
        assert_matches!(&entries[1].value, DataStoreValue::Canonical(value) if value == "03");
        assert!(lookup_map.is_empty());
    }

    #[test]
    fn rollback_removes_canonical_and_typed_pending_data_atomically() {
        let key = "vm::contract::0::entry".to_owned();
        let typed = typed_uint(1);
        let canonical = typed.canonical().to_owned();
        let mut lookup_map =
            HashMap::from([(key.clone(), vec![PendingDataValue::Typed(Box::new(typed))])]);

        let mut edits = Vec::new();
        rollback_edits_push(&mut edits, key.clone(), &canonical);
        let (_, check) = edits.pop().unwrap();
        let pending = rollback_data_lookup_map(&key, &check, &mut lookup_map).unwrap();
        assert_eq!(pending.canonical(), canonical);
        assert_matches!(
            pending,
            PendingDataValue::Typed(typed) if typed.value() == &Value::UInt(1)
        );
        assert!(lookup_map.is_empty());
    }

    #[test]
    fn pending_entry_keeps_typed_metadata_out_of_line() {
        assert_eq!(
            std::mem::size_of::<PendingDataValue>(),
            std::mem::size_of::<String>()
        );
    }
}
