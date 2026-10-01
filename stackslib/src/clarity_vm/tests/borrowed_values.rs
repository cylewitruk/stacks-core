// Copyright (C) 2026 Stacks Open Internet Foundation
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

//! Integration coverage for Clarity values read from Binary V1 packed storage.

use clarity::vm::ClarityVersion;
use clarity::vm::contexts::{AssetMap, ContractContext, OwnedEnvironment};
use clarity::vm::costs::{CostTracker, ExecutionCost, LimitedCostTracker};
use clarity::vm::database::clarity_store::StoredValue;
use clarity::vm::test_util::{TEST_BURN_STATE_DB, TEST_HEADER_DB, execute};
use clarity::vm::types::codec::packed::SharedPackedValue;
use clarity::vm::types::{PrincipalData, QualifiedContractIdentifier, Value};
use stacks_common::consts::{
    CHAIN_ID_TESTNET, FIRST_BURNCHAIN_CONSENSUS_HASH, FIRST_STACKS_BLOCK_HASH,
};
use stacks_common::types::StacksEpochId;
use stacks_common::types::chainstate::StacksBlockId;

use crate::chainstate::stacks::index::ClarityMarfTrieId as _;
use crate::clarity_vm::clarity::{ClarityMarfStore, ClarityMarfStoreTransaction};
use crate::clarity_vm::database::marf::MarfedKV;

/// Exercise packed tuples, lists, buffers, optionals, and responses through VM consumers.
#[test]
fn binary_values_remain_borrowed_across_vm_read_composition() {
    borrowed_read_composition(false);
}

/// Exercise the same borrowed VM operations with no SQL value storage.
#[test]
fn extent_values_remain_borrowed_across_vm_read_composition() {
    borrowed_read_composition(true);
}

/// Compare pending owned execution to persisted borrowed execution.
fn borrowed_read_composition(extents: bool) {
    const P1: &str = "'SZ2J6ZY48GV1EZ5V2V5RB9MP66SW86PYKKQ9H6DPR";
    let contract_id = QualifiedContractIdentifier::local("borrowed").unwrap();
    let payload = "ab".repeat(1_024);
    let contract = format!(
        r#"
        (define-data-var row
          (tuple (active bool) (items (list 4 uint)) (number (buff 2)) (payload (buff 1024)) (text (string-ascii 3)))
          {{ active: true, items: (list u1 u2 u3 u4), number: 0x0102, payload: 0x{payload}, text: "258" }})
        (define-map rows uint
          (tuple (active bool) (items (list 4 uint)) (number (buff 2)) (payload (buff 1024)) (text (string-ascii 3))))
        (map-set rows u7
          {{ active: true, items: (list u1 u2 u3 u4), number: 0x0102, payload: 0x{payload}, text: "258" }})

        (define-data-var compounds (list 2 (tuple (flag bool) (payload (buff 1024))))
          (list {{ flag: true, payload: 0x{payload} }} {{ flag: false, payload: 0x{payload} }}))
        (define-data-var nested (list 2 (list 2 (buff 1024)))
          (list (list 0x{payload} 0x{payload}) (list 0x{payload} 0x{payload})))
        (define-data-var buffers (list 2 (buff 1024)) (list 0x{payload} 0x{payload}))
        (define-data-var unicode (string-utf8 4) u"a\u{{e9}}\u{{1f600}}z")
        (define-private (sum-flags (item (tuple (flag bool) (payload (buff 1024)))) (total uint))
          (+ total (if (get flag item) u1 u0)))
        (define-private (flag-only (item (tuple (flag bool) (payload (buff 1024))))) (get flag item))
        (define-private (reject (item (tuple (flag bool) (payload (buff 1024))))) false)
        (define-private (sum-nested (item (list 2 (buff 1024))) (total uint)) (+ total (len item)))
        (define-private (sum-buffer (item (buff 1024)) (total uint)) (+ total (len item)))
        (define-private (sum-unicode (item (string-utf8 1)) (total uint)) (+ total (len item)))
        (define-private (increment (item uint)) (+ item u1))
        (define-private (keep-large (item uint)) (> item u2))
        (define-private (sum (item uint) (total uint)) (+ item total))

        (define-private (inspect-row
          (value (tuple (active bool) (items (list 4 uint)) (number (buff 2)) (payload (buff 1024)) (text (string-ascii 3)))))
          (let ((items (begin
                         (asserts! (get active value) u999)
                         (if (get active value) (get items value) (list))))
                (payload-bytes (get payload value)))
            (tuple
              (read-only-call (len (contract-call? .borrowed-relay echo payload-bytes)))
              (public-call (len (unwrap-panic (contract-call? .borrowed-relay echo-public payload-bytes))))
              (schema-cast (len (contract-call? .borrowed-relay widen (var-get buffers))))
              (filtered-fold (fold sum-flags (filter flag-only (var-get compounds)) u0))
              (sliced-fold (fold sum-flags (unwrap-panic (slice? (var-get compounds) u0 u1)) u0))
              (projected-buffer-len (len (unwrap-panic (get payload (element-at? (var-get compounds) u0)))))
              (nested-element-len (len (unwrap-panic (element-at? (var-get nested) u0))))
              (comparison (< (get payload value) 0xff))
              (compound-fold (fold sum-flags (var-get compounds) u0))
              (compound-map (map flag-only (var-get compounds)))
              (compound-filter (len (filter reject (var-get compounds))))
              (nested-fold (fold sum-nested (var-get nested) u0))
              (buffer-fold (fold sum-buffer (var-get buffers) u0))
              (unicode-fold (fold sum-unicode (var-get unicode) u0))
              (active (is-eq (get active value) true))
              (digest (sha256 payload-bytes))
              (equal (is-eq items (list u1 u2 u3 u4)))
              (filtered (len (filter keep-large items)))
              (length (len payload-bytes))
              (mapped (map increment items))
              (number (buff-to-uint-be (get number value)))
              (parsed (unwrap-panic (string-to-uint? (get text value))))
              (sum (fold sum items u0)))))

        (define-public (read-all)
          (match (map-get? rows u7)
            entry
              (ok (tuple
                (entry (inspect-row entry))
                (variable (inspect-row (var-get row)))))
            (err u404)))
        "#
    );

    let genesis = StacksBlockId::new(&FIRST_BURNCHAIN_CONSENSUS_HASH, &FIRST_STACKS_BLOCK_HASH);
    let deployment = StacksBlockId([1; 32]);
    let read_block = StacksBlockId([2; 32]);
    let mut marf = MarfedKV::temporary();
    if extents {
        marf.enable_value_extents().unwrap();
    }
    let Value::Principal(PrincipalData::Standard(sender)) = execute(P1) else {
        panic!("test sender must be a standard principal")
    };

    {
        let mut store = marf.begin(&StacksBlockId::sentinel(), &genesis);
        store
            .as_clarity_db(&TEST_HEADER_DB, &TEST_BURN_STATE_DB)
            .initialize();
        store.test_commit();
    }

    let (owned_outcome, owned_cost, owned_memory) = {
        let mut store = marf.begin(&genesis, &deployment);
        let mut env = OwnedEnvironment::new_cost_limited(
            false,
            CHAIN_ID_TESTNET,
            store.as_clarity_db(&TEST_HEADER_DB, &TEST_BURN_STATE_DB),
            LimitedCostTracker::new_with_limit(StacksEpochId::latest(), ExecutionCost::max_value()),
            StacksEpochId::latest(),
        );
        env.initialize_versioned_contract(
            QualifiedContractIdentifier::local("borrowed-relay").unwrap(),
            ClarityVersion::latest(),
            r#"(define-read-only (echo (value (buff 4096))) value)
               (define-public (echo-public (value (buff 4096))) (ok value))
               (define-read-only (widen (value (list 4 (buff 4096)))) value)"#,
            None,
        )
        .unwrap();
        env.initialize_versioned_contract(
            contract_id.clone(),
            ClarityVersion::latest(),
            &contract,
            None,
        )
        .unwrap();
        env.mut_cost_tracker().set_total(ExecutionCost::ZERO);
        env.mut_cost_tracker().reset_memory();
        let outcome = env
            .execute_transaction(
                sender.clone().into(),
                None,
                contract_id.clone(),
                "read-all",
                &[],
            )
            .unwrap();
        let cost = env.get_cost_total();
        let memory = env.mut_cost_tracker().get_memory();
        drop(env);
        store.test_commit();
        (outcome, cost, memory)
    };

    {
        let mut store = marf.begin(&deployment, &read_block);
        let mut db = store.as_clarity_db(&TEST_HEADER_DB, &TEST_BURN_STATE_DB);
        db.begin();
        let descriptor = db.load_variable(&contract_id, "row").unwrap();
        let stored = db
            .lookup_variable_stored_with_size(
                &contract_id,
                "row",
                &descriptor,
                &StacksEpochId::latest(),
            )
            .unwrap();
        let StoredValue::Packed(packed) = stored.value else {
            panic!("Binary V1 read must retain shared packed storage")
        };
        assert!(!packed.is_materialized());
        assert_eq!(packed.tuple_len().unwrap(), 5);
        assert_eq!(
            packed
                .tuple_field("payload")
                .unwrap()
                .unwrap()
                .as_view()
                .as_sequence_bytes()
                .unwrap()
                .len(),
            1_024
        );
        assert!(!packed.is_materialized());
        db.roll_back().unwrap();
        drop(db);
        store.drop_current_trie();
    }

    {
        let mut store = marf.begin(&deployment, &read_block);
        let mut env = OwnedEnvironment::new_cost_limited(
            false,
            CHAIN_ID_TESTNET,
            store.as_clarity_db(&TEST_HEADER_DB, &TEST_BURN_STATE_DB),
            LimitedCostTracker::new_with_limit(StacksEpochId::latest(), ExecutionCost::max_value()),
            StacksEpochId::latest(),
        );
        SharedPackedValue::reset_materialization_count();
        let (result, assets, events) = env
            .execute_transaction(sender.into(), None, contract_id.clone(), "read-all", &[])
            .unwrap();
        assert_eq!(
            (result.clone(), assets.clone(), events.clone()),
            owned_outcome
        );

        let expected = execute(&format!(
            r#"(ok (tuple
              (entry (tuple
                (read-only-call u1024)
                (public-call u1024)
                (schema-cast u2)
                (filtered-fold u1)
                (sliced-fold u1)
                (projected-buffer-len u1024)
                (nested-element-len u2)
                (comparison true)
                (compound-fold u1)
                (compound-map (list true false))
                (compound-filter u0)
                (nested-fold u4)
                (buffer-fold u2048)
                (unicode-fold u4)
                (active true)
                (digest (sha256 0x{payload}))
                (equal true)
                (filtered u2)
                (length u1024)
                (mapped (list u2 u3 u4 u5))
                (number u258)
                (parsed u258)
                (sum u10)))
              (variable (tuple
                (read-only-call u1024)
                (public-call u1024)
                (schema-cast u2)
                (filtered-fold u1)
                (sliced-fold u1)
                (projected-buffer-len u1024)
                (nested-element-len u2)
                (comparison true)
                (compound-fold u1)
                (compound-map (list true false))
                (compound-filter u0)
                (nested-fold u4)
                (buffer-fold u2048)
                (unicode-fold u4)
                (active true)
                (digest (sha256 0x{payload}))
                (equal true)
                (filtered u2)
                (length u1024)
                (mapped (list u2 u3 u4 u5))
                (number u258)
                (parsed u258)
                (sum u10)))))"#
        ));
        assert_eq!(result, expected);
        assert_eq!(assets, AssetMap::new());
        assert!(events.is_empty());
        assert_eq!(env.get_cost_total(), owned_cost);
        assert_eq!(env.mut_cost_tracker().get_memory(), owned_memory);
        assert_eq!(
            SharedPackedValue::materialization_count(),
            0,
            "unexpected materializations: {:?}",
            SharedPackedValue::materialization_locations()
        );

        {
            let placeholder = ContractContext::new(
                QualifiedContractIdentifier::transient(),
                ClarityVersion::latest(),
            );
            let (mut exec_state, invoke_ctx) = env.get_exec_environment(None, None, &placeholder);
            let direct = exec_state
                .eval_read_only(&invoke_ctx, &contract_id, "(var-get row)")
                .unwrap();
            assert!(direct.serialize_to_vec().unwrap().len() > payload.len() / 2);
        }
        drop(env);
        store.drop_current_trie();
    }
}

/// Prove that pending-owned and committed-packed reads drive identical committed state.
#[test]
fn pending_and_packed_reads_produce_identical_state_roots() {
    compare_state_roots(false);
}

/// Compare SQL-owned execution against mapped extent reads under identical logical state.
#[test]
fn sql_and_extent_reads_produce_identical_state_roots() {
    compare_state_roots(true);
}

/// Execute both storage arms using the same block and contract identities.
fn compare_state_roots(extents: bool) {
    const P1: &str = "'SZ2J6ZY48GV1EZ5V2V5RB9MP66SW86PYKKQ9H6DPR";
    let contract_id = QualifiedContractIdentifier::local("borrowed-root").unwrap();
    let payload = "cd".repeat(256);
    let row = format!("{{ amount: u7, payload: 0x{payload} }}");
    let contract = format!(
        r#"
        (define-data-var source (tuple (amount uint) (payload (buff 256))) {row})
        (define-data-var sink uint u0)
        (define-data-var composed (list 4 (buff 512)) (list))
        (define-map copies (buff 256) (tuple (amount uint) (payload (buff 256))))
        (define-private (write-composite)
          (let ((row (var-get source)) (bytes (get payload row)))
            (begin
              (var-set composed (append (list bytes) (unwrap-panic (as-max-len? (concat bytes bytes) u512))))
              (map-set copies bytes (merge row {{ amount: u9 }}))
              (asserts! (not (map-insert copies bytes row)) false)
              (asserts! (map-delete copies bytes) false)
              (map-set copies bytes row)
              true)))
        (define-private (derived)
          (+ (get amount (var-get source)) (len (get payload (var-get source)))))
        (define-public (pending-arm)
          (begin
            (var-set source {row})
            (write-composite)
            (var-set sink (derived))
            (ok (var-get sink))))
        (define-public (packed-arm)
          (let ((value (derived)))
            (write-composite)
            (var-set source {row})
            (var-set sink value)
            (ok (var-get sink))))
        "#
    );
    let genesis = StacksBlockId::new(&FIRST_BURNCHAIN_CONSENSUS_HASH, &FIRST_STACKS_BLOCK_HASH);
    let deployment = StacksBlockId([11; 32]);
    let execution = StacksBlockId([12; 32]);
    let Value::Principal(PrincipalData::Standard(sender)) = execute(P1) else {
        panic!("test sender must be a standard principal")
    };

    fn deploy(
        genesis: &StacksBlockId,
        deployment: &StacksBlockId,
        contract_id: &QualifiedContractIdentifier,
        contract: &str,
        extents: bool,
    ) -> MarfedKV {
        let mut marf = MarfedKV::temporary();
        if extents {
            marf.enable_value_extents().unwrap();
        }
        {
            let mut store = marf.begin(&StacksBlockId::sentinel(), genesis);
            store
                .as_clarity_db(&TEST_HEADER_DB, &TEST_BURN_STATE_DB)
                .initialize();
            store.test_commit();
        }
        {
            let mut store = marf.begin(genesis, deployment);
            let mut env = OwnedEnvironment::new_cost_limited(
                false,
                CHAIN_ID_TESTNET,
                store.as_clarity_db(&TEST_HEADER_DB, &TEST_BURN_STATE_DB),
                LimitedCostTracker::new_with_limit(
                    StacksEpochId::latest(),
                    ExecutionCost::max_value(),
                ),
                StacksEpochId::latest(),
            );
            env.initialize_versioned_contract(
                contract_id.clone(),
                ClarityVersion::latest(),
                contract,
                None,
            )
            .unwrap();
            drop(env);
            store.test_commit();
        }
        marf
    }

    fn execute_and_seal(
        marf: &mut MarfedKV,
        deployment: &StacksBlockId,
        execution: &StacksBlockId,
        sender: PrincipalData,
        contract_id: &QualifiedContractIdentifier,
        function: &str,
    ) -> (
        (
            Value,
            AssetMap,
            Vec<clarity::vm::events::StacksTransactionEvent>,
        ),
        [u8; 32],
    ) {
        let mut store = marf.begin(deployment, execution);
        let mut env = OwnedEnvironment::new_cost_limited(
            false,
            CHAIN_ID_TESTNET,
            store.as_clarity_db(&TEST_HEADER_DB, &TEST_BURN_STATE_DB),
            LimitedCostTracker::new_with_limit(StacksEpochId::latest(), ExecutionCost::max_value()),
            StacksEpochId::latest(),
        );
        SharedPackedValue::reset_materialization_count();
        let outcome = env
            .execute_transaction(sender, None, contract_id.clone(), function, &[])
            .unwrap();
        assert_eq!(
            SharedPackedValue::materialization_count(),
            0,
            "write boundary materialized: {:?}",
            SharedPackedValue::materialization_locations()
        );
        drop(env);
        let root = store.seal_trie();
        store.drop_current_trie();
        (outcome, *root.as_bytes())
    }

    let mut pending = deploy(&genesis, &deployment, &contract_id, &contract, false);
    let mut packed = deploy(&genesis, &deployment, &contract_id, &contract, extents);
    let pending_outcome = execute_and_seal(
        &mut pending,
        &deployment,
        &execution,
        sender.clone().into(),
        &contract_id,
        "pending-arm",
    );
    let packed_outcome = execute_and_seal(
        &mut packed,
        &deployment,
        &execution,
        sender.into(),
        &contract_id,
        "packed-arm",
    );

    assert_eq!(pending_outcome, packed_outcome);
}
