#[cfg(test)]
mod tests {
    use stacks_common::deps_common::bitcoin::blockdata::block::{BlockHeader, LoneBlockHeader};
    use stacks_common::deps_common::bitcoin::network::encodable::VarInt;
    use stacks_common::deps_common::bitcoin::network::serialize::BitcoinHash;
    use stacks_common::deps_common::bitcoin::util::hash::Sha256dHash;
    use stacks_common::types::chainstate::{
        BurnchainHeaderHash, ConsensusHash, PoxId, SortitionId,
    };
    use stacks_common::types::StacksEpochId;

    use super::*;
    use crate::burnchains::bitcoin::spv::SpvClient;
    use crate::burnchains::bitcoin::BitcoinNetworkType;

    struct DummyBurnStateDb {
        tip_height: u32,
        burn_header_hash: BurnchainHeaderHash,
        tip_sortition_id: SortitionId,
    }

    impl DummyBurnStateDb {
        fn new(tip_height: u32, burn_header_hash: BurnchainHeaderHash) -> Self {
            let tip_sortition_id = SortitionId::new(&burn_header_hash, &PoxId::stubbed());
            Self {
                tip_height,
                burn_header_hash,
                tip_sortition_id,
            }
        }
    }

    impl BurnStateDB for DummyBurnStateDb {
        fn get_tip_burn_block_height(&self) -> Option<u32> {
            Some(self.tip_height)
        }

        fn get_tip_sortition_id(&self) -> Option<SortitionId> {
            Some(self.tip_sortition_id.clone())
        }

        fn get_v1_unlock_height(&self) -> u32 {
            u32::MAX
        }

        fn get_v2_unlock_height(&self) -> u32 {
            u32::MAX
        }

        fn get_v3_unlock_height(&self) -> u32 {
            u32::MAX
        }

        fn get_pox_3_activation_height(&self) -> u32 {
            u32::MAX
        }

        fn get_pox_4_activation_height(&self) -> u32 {
            u32::MAX
        }

        fn get_burn_block_height(&self, _sortition_id: &SortitionId) -> Option<u32> {
            Some(self.tip_height)
        }

        fn get_burn_start_height(&self) -> u32 {
            0
        }

        fn get_pox_prepare_length(&self) -> u32 {
            1
        }

        fn get_pox_reward_cycle_length(&self) -> u32 {
            1
        }

        fn get_pox_rejection_fraction(&self) -> u64 {
            1
        }

        fn get_burn_header_hash(
            &self,
            height: u32,
            _sortition_id: &SortitionId,
        ) -> Option<BurnchainHeaderHash> {
            if height == self.tip_height {
                Some(self.burn_header_hash.clone())
            } else {
                None
            }
        }

        fn get_sortition_id_from_consensus_hash(
            &self,
            _consensus_hash: &ConsensusHash,
        ) -> Option<SortitionId> {
            Some(self.tip_sortition_id.clone())
        }

        fn get_stacks_epoch(&self, _height: u32) -> Option<StacksEpoch> {
            None
        }

        fn get_stacks_epoch_by_epoch_id(&self, _epoch_id: &StacksEpochId) -> Option<StacksEpoch> {
            None
        }

        fn get_pox_payout_addrs(
            &self,
            _height: u32,
            _sortition_id: &SortitionId,
        ) -> Option<(Vec<TupleData>, u128)> {
            None
        }
    }

    fn setup_spv_db(path: &str) -> (BurnchainHeaderHash, Vec<u8>) {
        let header = LoneBlockHeader {
            header: BlockHeader {
                bits: 545259519,
                merkle_root: Sha256dHash::from_hex(
                    "20bee96458517fc5082a9720ce6207b5742f2b18e4e0a7e7373342725d80f88c",
                )
                .unwrap(),
                nonce: 2,
                prev_blockhash: Sha256dHash::from_hex(
                    "0f9188f13cb7b2c71f2a335e3a4fc328bf5beb436012afca590b1a11466e2206",
                )
                .unwrap(),
                time: 1587626881,
                version: 0x20000000,
            },
            tx_count: VarInt(0),
        };

        let mut spv_client =
            SpvClient::new(path, 0, None, BitcoinNetworkType::Regtest, true, false)
                .expect("failed to open spv db");
        spv_client
            .write_block_headers(1, vec![header.clone()])
            .unwrap();

        let burn_header_hash =
            BurnchainHeaderHash::from_bitcoin_hash(&header.header.bitcoin_hash());
        (
            burn_header_hash,
            header.header.merkle_root.as_bytes().to_vec(),
        )
    }

    #[test]
    fn test_burn_state_db_with_spv_merkle_root_lookup() {
        let path = "/tmp/test-burnstate-with-spv.sqlite";
        if std::fs::metadata(path).is_ok() {
            std::fs::remove_file(path).unwrap();
        }

        let (burn_header_hash, merkle_root) = setup_spv_db(path);
        let burn_state_db = DummyBurnStateDb::new(7, burn_header_hash.clone());
        let wrapper = BurnStateDBWithSpv::open(&burn_state_db, path)
            .expect("failed to open burn state wrapper");

        assert_eq!(wrapper.get_tip_burn_block_height(), Some(7));
        assert_eq!(wrapper.get_spv_header_height(&burn_header_hash), Some(1));
        assert_eq!(
            wrapper.get_spv_header_merkle_root(&burn_header_hash),
            Some(merkle_root)
        );
    }
}
use std::ops::Deref;

use clarity::types::chainstate::TrieHash;
use clarity::util::hash::Sha512Trunc256Sum;
use clarity::vm::analysis::AnalysisDatabase;
use clarity::vm::database::sqlite::{
    sqlite_get_contract_hash, sqlite_get_metadata, sqlite_get_metadata_manual,
    sqlite_insert_metadata,
};
use clarity::vm::database::{
    BurnStateDB, ClarityBackingStore, ClarityDatabase, HeadersDB, SpecialCaseHandler,
    SqliteConnection, NULL_BURN_STATE_DB, NULL_HEADER_DB,
};
use clarity::vm::errors::{RuntimeError, VmExecutionError};
use clarity::vm::types::{QualifiedContractIdentifier, TupleData};
use rusqlite::{params, Connection, OptionalExtension, Row};
use stacks_common::types::chainstate::{
    BlockHeaderHash, BurnchainHeaderHash, ConsensusHash, SortitionId, StacksAddress, StacksBlockId,
    TenureBlockId, VRFSeed,
};
use stacks_common::types::Address;
use stacks_common::util::vrf::VRFProof;

use crate::burnchains::bitcoin::spv::SpvHeadersDb;
use crate::burnchains::bitcoin::Error as btc_error;
use crate::chainstate::burn::db::sortdb::{
    get_ancestor_sort_id, SortitionDB, SortitionHandle, SortitionHandleConn, SortitionHandleTx,
};
use crate::chainstate::nakamoto::{keys as nakamoto_keys, StacksDBIndexed};
use crate::chainstate::stacks::boot::PoxStartCycleInfo;
use crate::chainstate::stacks::db::accounts::MinerReward;
use crate::chainstate::stacks::db::{ChainstateTx, StacksChainState, StacksDBConn, StacksDBTx};
use crate::chainstate::stacks::index::marf::{MarfConnection, MARF};
use crate::chainstate::stacks::index::ClarityMarfTrieId;
use crate::chainstate::stacks::Error as ChainstateError;
use crate::clarity_vm::special::handle_contract_call_special_cases;
use crate::core::{StacksEpoch, StacksEpochId};
use crate::util_lib::db::{DBConn, Error as DBError, FromColumn, FromRow};

pub mod ephemeral;
pub mod marf;

/// BurnStateDB wrapper that provides SPV header lookups.
pub struct BurnStateDBWithSpv<'a, B: BurnStateDB + ?Sized> {
    inner: &'a B,
    spv_headers_db: SpvHeadersDb,
}

impl<'a, B: BurnStateDB + ?Sized> BurnStateDBWithSpv<'a, B> {
    pub fn open(inner: &'a B, spv_headers_path: &str) -> Result<Self, btc_error> {
        Ok(Self {
            inner,
            spv_headers_db: SpvHeadersDb::open_readonly(spv_headers_path)?,
        })
    }

    pub fn inner(&self) -> &'a B {
        self.inner
    }
}

impl<B: BurnStateDB + ?Sized> BurnStateDB for BurnStateDBWithSpv<'_, B> {
    fn get_tip_burn_block_height(&self) -> Option<u32> {
        self.inner.get_tip_burn_block_height()
    }

    fn get_tip_sortition_id(&self) -> Option<SortitionId> {
        self.inner.get_tip_sortition_id()
    }

    fn get_v1_unlock_height(&self) -> u32 {
        self.inner.get_v1_unlock_height()
    }

    fn get_v2_unlock_height(&self) -> u32 {
        self.inner.get_v2_unlock_height()
    }

    fn get_v3_unlock_height(&self) -> u32 {
        self.inner.get_v3_unlock_height()
    }

    fn get_pox_3_activation_height(&self) -> u32 {
        self.inner.get_pox_3_activation_height()
    }

    fn get_pox_4_activation_height(&self) -> u32 {
        self.inner.get_pox_4_activation_height()
    }

    fn get_burn_block_height(&self, sortition_id: &SortitionId) -> Option<u32> {
        self.inner.get_burn_block_height(sortition_id)
    }

    fn get_burn_start_height(&self) -> u32 {
        self.inner.get_burn_start_height()
    }

    fn get_pox_prepare_length(&self) -> u32 {
        self.inner.get_pox_prepare_length()
    }

    fn get_pox_reward_cycle_length(&self) -> u32 {
        self.inner.get_pox_reward_cycle_length()
    }

    fn get_pox_rejection_fraction(&self) -> u64 {
        self.inner.get_pox_rejection_fraction()
    }

    fn get_burn_header_hash(
        &self,
        height: u32,
        sortition_id: &SortitionId,
    ) -> Option<BurnchainHeaderHash> {
        self.inner.get_burn_header_hash(height, sortition_id)
    }

    fn get_burn_block_height_for_header_hash(
        &self,
        burn_header_hash: &BurnchainHeaderHash,
        sortition_id: &SortitionId,
    ) -> Option<u32> {
        self.inner
            .get_burn_block_height_for_header_hash(burn_header_hash, sortition_id)
    }

    fn get_sortition_id_from_consensus_hash(
        &self,
        consensus_hash: &ConsensusHash,
    ) -> Option<SortitionId> {
        self.inner
            .get_sortition_id_from_consensus_hash(consensus_hash)
    }

    fn get_stacks_epoch(&self, height: u32) -> Option<StacksEpoch> {
        self.inner.get_stacks_epoch(height)
    }

    fn get_stacks_epoch_by_epoch_id(&self, epoch_id: &StacksEpochId) -> Option<StacksEpoch> {
        self.inner.get_stacks_epoch_by_epoch_id(epoch_id)
    }

    fn get_pox_payout_addrs(
        &self,
        height: u32,
        sortition_id: &SortitionId,
    ) -> Option<(Vec<TupleData>, u128)> {
        self.inner.get_pox_payout_addrs(height, sortition_id)
    }

    fn get_spv_header_height(&self, burn_header_hash: &BurnchainHeaderHash) -> Option<u64> {
        self.spv_headers_db
            .get_header_height(burn_header_hash)
            .ok()
            .flatten()
    }

    fn get_spv_header_merkle_root(
        &self,
        burn_header_hash: &BurnchainHeaderHash,
    ) -> Option<Vec<u8>> {
        self.spv_headers_db
            .get_merkle_root(burn_header_hash)
            .ok()
            .flatten()
    }
}

pub trait GetTenureStartId {
    fn get_tenure_block_id(
        &self,
        tip: &StacksBlockId,
        tenure_id_consensus_hash: &ConsensusHash,
    ) -> Result<Option<TenureBlockId>, DBError>;
    /// Return the StacksBlockId of the tenure start block for the
    ///  tenure with coinbase height `coinbase_height` in the fork
    ///  referenced by `tip`.
    fn get_tenure_block_id_at_cb_height(
        &self,
        tip: &StacksBlockId,
        coinbase_height: u64,
    ) -> Result<Option<StacksBlockId>, DBError>;
    fn conn(&self) -> &Connection;
}

impl GetTenureStartId for StacksDBConn<'_> {
    fn get_tenure_block_id(
        &self,
        tip: &StacksBlockId,
        tenure_id_consensus_hash: &ConsensusHash,
    ) -> Result<Option<TenureBlockId>, DBError> {
        Ok(self
            .get_indexed(
                tip,
                &nakamoto_keys::tenure_start_block_id(tenure_id_consensus_hash),
            )?
            .and_then(|id_str| nakamoto_keys::parse_block_id(&id_str))
            .map(TenureBlockId::from))
    }

    fn get_tenure_block_id_at_cb_height(
        &self,
        tip: &StacksBlockId,
        coinbase_height: u64,
    ) -> Result<Option<StacksBlockId>, DBError> {
        let opt_out = self
            .get_indexed(
                tip,
                &nakamoto_keys::ongoing_tenure_coinbase_height(coinbase_height),
            )?
            .and_then(|hex_inp| nakamoto_keys::parse_block_id(&hex_inp));
        Ok(opt_out)
    }

    fn conn(&self) -> &Connection {
        self.sqlite()
    }
}

impl GetTenureStartId for StacksDBTx<'_> {
    fn get_tenure_block_id(
        &self,
        tip: &StacksBlockId,
        tenure_id_consensus_hash: &ConsensusHash,
    ) -> Result<Option<TenureBlockId>, DBError> {
        Ok(self
            .get_indexed_ref(
                tip,
                &nakamoto_keys::tenure_start_block_id(tenure_id_consensus_hash),
            )?
            .and_then(|id_str| nakamoto_keys::parse_block_id(&id_str))
            .map(TenureBlockId::from))
    }

    fn get_tenure_block_id_at_cb_height(
        &self,
        tip: &StacksBlockId,
        coinbase_height: u64,
    ) -> Result<Option<StacksBlockId>, DBError> {
        let opt_out = self
            .get_indexed_ref(
                tip,
                &nakamoto_keys::ongoing_tenure_coinbase_height(coinbase_height),
            )?
            .and_then(|hex_inp| nakamoto_keys::parse_block_id(&hex_inp));
        Ok(opt_out)
    }

    fn conn(&self) -> &Connection {
        self.sqlite()
    }
}

impl GetTenureStartId for MARF<StacksBlockId> {
    fn get_tenure_block_id(
        &self,
        tip: &StacksBlockId,
        tenure_id_consensus_hash: &ConsensusHash,
    ) -> Result<Option<TenureBlockId>, DBError> {
        let dbconn = StacksDBConn::new(self, ());
        dbconn.get_tenure_block_id(tip, tenure_id_consensus_hash)
    }

    fn conn(&self) -> &Connection {
        self.sqlite_conn()
    }

    fn get_tenure_block_id_at_cb_height(
        &self,
        tip: &StacksBlockId,
        coinbase_height: u64,
    ) -> Result<Option<StacksBlockId>, DBError> {
        let dbconn = StacksDBConn::new(self, ());
        dbconn.get_tenure_block_id_at_cb_height(tip, coinbase_height)
    }
}

pub struct HeadersDBConn<'a>(pub StacksDBConn<'a>);

impl HeadersDB for HeadersDBConn<'_> {
    fn get_stacks_block_header_hash_for_block(
        &self,
        id_bhh: &StacksBlockId,
        epoch: &StacksEpochId,
    ) -> Option<BlockHeaderHash> {
        get_stacks_header_column_from_table(
            self.0.conn(),
            id_bhh,
            "block_hash",
            &|r| {
                BlockHeaderHash::from_column(r, "block_hash").expect("FATAL: malformed block hash")
            },
            epoch.uses_nakamoto_blocks(),
        )
    }

    fn get_burn_header_hash_for_block(
        &self,
        id_bhh: &StacksBlockId,
    ) -> Option<BurnchainHeaderHash> {
        get_stacks_header_column(self.0.conn(), id_bhh, "burn_header_hash", |r| {
            BurnchainHeaderHash::from_row(r).expect("FATAL: malformed burn_header_hash")
        })
    }

    fn get_consensus_hash_for_block(
        &self,
        id_bhh: &StacksBlockId,
        epoch: &StacksEpochId,
    ) -> Option<ConsensusHash> {
        get_stacks_header_column_from_table(
            self.0.conn(),
            id_bhh,
            "consensus_hash",
            &|r| ConsensusHash::from_row(r).expect("FATAL: malformed consensus_hash"),
            epoch.uses_nakamoto_blocks(),
        )
    }

    fn get_burn_block_time_for_block(
        &self,
        id_bhh: &StacksBlockId,
        epoch_opt: Option<&StacksEpochId>,
    ) -> Option<u64> {
        if let Some(epoch) = epoch_opt {
            get_stacks_header_column_from_table(
                self.0.conn(),
                id_bhh,
                "burn_header_timestamp",
                &|r| u64::from_row(r).expect("FATAL: malformed burn_header_timestamp"),
                epoch.uses_nakamoto_blocks(),
            )
        } else {
            get_stacks_header_column(self.0.conn(), id_bhh, "burn_header_timestamp", |r| {
                u64::from_row(r).expect("FATAL: malformed burn_header_timestamp")
            })
        }
    }

    fn get_stacks_block_time_for_block(&self, id_bhh: &StacksBlockId) -> Option<u64> {
        get_stacks_header_column_from_table(
            self.0.conn(),
            id_bhh,
            "timestamp",
            &|r| u64::from_row(r).expect("FATAL: malformed timestamp"),
            true,
        )
    }

    fn get_burn_block_height_for_block(&self, id_bhh: &StacksBlockId) -> Option<u32> {
        get_stacks_header_column(self.0.conn(), id_bhh, "burn_header_height", |r| {
            u64::from_row(r)
                .expect("FATAL: malformed burn_header_height")
                .try_into()
                .expect("FATAL: blockchain too long")
        })
    }

    fn get_stacks_height_for_tenure_height(
        &self,
        tip: &StacksBlockId,
        tenure_height: u32,
    ) -> Option<u32> {
        let tenure_block_id =
            GetTenureStartId::get_tenure_block_id_at_cb_height(&self.0, tip, tenure_height.into())
                .expect("FATAL: bad DB data for tenure height lookups")?;
        get_stacks_header_column(self.0.conn(), &tenure_block_id, "block_height", |r| {
            u64::from_row(r)
                .expect("FATAL: malformed block_height")
                .try_into()
                .expect("FATAL: blockchain too long")
        })
    }

    fn get_vrf_seed_for_block(
        &self,
        id_bhh: &StacksBlockId,
        epoch: &StacksEpochId,
    ) -> Option<VRFSeed> {
        let tenure_id_bhh = get_first_block_in_tenure(&self.0, id_bhh, Some(epoch));
        let (column_name, nakamoto) = if epoch.uses_nakamoto_blocks() {
            ("vrf_proof", true)
        } else {
            ("proof", false)
        };
        get_stacks_header_column_from_table(
            self.0.conn(),
            &tenure_id_bhh.0,
            column_name,
            &|r| {
                let proof = VRFProof::from_column(r, column_name).expect("FATAL: malformed proof");
                VRFSeed::from_proof(&proof)
            },
            nakamoto,
        )
    }

    fn get_miner_address(
        &self,
        id_bhh: &StacksBlockId,
        epoch: &StacksEpochId,
    ) -> Option<StacksAddress> {
        let tenure_id_bhh = get_first_block_in_tenure(&self.0, id_bhh, Some(epoch));
        get_miner_column(self.0.conn(), &tenure_id_bhh, "address", |r| {
            let s: String = r.get_unwrap("address");
            let addr = StacksAddress::from_string(&s).expect("FATAL: malformed address");
            addr
        })
    }

    fn get_burnchain_tokens_spent_for_block(
        &self,
        id_bhh: &StacksBlockId,
        epoch: &StacksEpochId,
    ) -> Option<u128> {
        let tenure_id_bhh = get_first_block_in_tenure(&self.0, id_bhh, Some(epoch));
        get_miner_column(
            self.0.conn(),
            &tenure_id_bhh,
            "burnchain_sortition_burn",
            |r| u64::from_row(r).expect("FATAL: malformed sortition burn"),
        )
        .map(|x| x.into())
    }

    fn get_burnchain_tokens_spent_for_winning_block(
        &self,
        id_bhh: &StacksBlockId,
        epoch: &StacksEpochId,
    ) -> Option<u128> {
        let tenure_id_bhh = get_first_block_in_tenure(&self.0, id_bhh, Some(epoch));
        get_miner_column(
            self.0.conn(),
            &tenure_id_bhh,
            "burnchain_commit_burn",
            |r| u64::from_row(r).expect("FATAL: malformed commit burn"),
        )
        .map(|x| x.into())
    }

    fn get_tokens_earned_for_block(
        &self,
        id_bhh: &StacksBlockId,
        epoch: &StacksEpochId,
    ) -> Option<u128> {
        let tenure_id_bhh = get_first_block_in_tenure(&self.0, id_bhh, Some(epoch));
        get_matured_reward(&self.0, &tenure_id_bhh, epoch).map(|x| x.total())
    }
}

impl HeadersDB for ChainstateTx<'_> {
    fn get_stacks_block_header_hash_for_block(
        &self,
        id_bhh: &StacksBlockId,
        epoch: &StacksEpochId,
    ) -> Option<BlockHeaderHash> {
        get_stacks_header_column_from_table(
            self.deref().deref(),
            id_bhh,
            "block_hash",
            &|r| {
                BlockHeaderHash::from_column(r, "block_hash").expect("FATAL: malformed block hash")
            },
            epoch.uses_nakamoto_blocks(),
        )
    }

    fn get_burn_header_hash_for_block(
        &self,
        id_bhh: &StacksBlockId,
    ) -> Option<BurnchainHeaderHash> {
        get_stacks_header_column(self.deref().deref(), id_bhh, "burn_header_hash", |r| {
            BurnchainHeaderHash::from_row(r).expect("FATAL: malformed burn_header_hash")
        })
    }

    fn get_consensus_hash_for_block(
        &self,
        id_bhh: &StacksBlockId,
        epoch: &StacksEpochId,
    ) -> Option<ConsensusHash> {
        get_stacks_header_column_from_table(
            self.deref().deref(),
            id_bhh,
            "consensus_hash",
            &|r| ConsensusHash::from_row(r).expect("FATAL: malformed consensus_hash"),
            epoch.uses_nakamoto_blocks(),
        )
    }

    fn get_burn_block_time_for_block(
        &self,
        id_bhh: &StacksBlockId,
        epoch_opt: Option<&StacksEpochId>,
    ) -> Option<u64> {
        if let Some(epoch) = epoch_opt {
            get_stacks_header_column_from_table(
                self.deref().deref(),
                id_bhh,
                "burn_header_timestamp",
                &|r| u64::from_row(r).expect("FATAL: malformed burn_header_timestamp"),
                epoch.uses_nakamoto_blocks(),
            )
        } else {
            get_stacks_header_column(self.deref().deref(), id_bhh, "burn_header_timestamp", |r| {
                u64::from_row(r).expect("FATAL: malformed burn_header_timestamp")
            })
        }
    }

    fn get_stacks_block_time_for_block(&self, id_bhh: &StacksBlockId) -> Option<u64> {
        get_stacks_header_column_from_table(
            self.deref().deref(),
            id_bhh,
            "timestamp",
            &|r| u64::from_row(r).expect("FATAL: malformed timestamp"),
            true,
        )
    }

    fn get_burn_block_height_for_block(&self, id_bhh: &StacksBlockId) -> Option<u32> {
        get_stacks_header_column(self.deref().deref(), id_bhh, "burn_header_height", |r| {
            u64::from_row(r)
                .expect("FATAL: malformed burn_header_height")
                .try_into()
                .expect("FATAL: blockchain too long")
        })
    }

    fn get_vrf_seed_for_block(
        &self,
        id_bhh: &StacksBlockId,
        epoch: &StacksEpochId,
    ) -> Option<VRFSeed> {
        let tenure_id_bhh = get_first_block_in_tenure(self.deref(), id_bhh, Some(epoch));
        let (column_name, nakamoto) = if epoch.uses_nakamoto_blocks() {
            ("vrf_proof", true)
        } else {
            ("proof", false)
        };
        get_stacks_header_column_from_table(
            self.deref().deref(),
            &tenure_id_bhh.0,
            column_name,
            &|r| {
                let proof = VRFProof::from_column(r, column_name).expect("FATAL: malformed proof");
                VRFSeed::from_proof(&proof)
            },
            nakamoto,
        )
    }

    fn get_miner_address(
        &self,
        id_bhh: &StacksBlockId,
        epoch: &StacksEpochId,
    ) -> Option<StacksAddress> {
        let tenure_id_bhh = get_first_block_in_tenure(self.deref(), id_bhh, Some(epoch));
        get_miner_column(self.deref().deref(), &tenure_id_bhh, "address", |r| {
            let s: String = r.get_unwrap("address");
            let addr = StacksAddress::from_string(&s).expect("FATAL: malformed address");
            addr
        })
    }

    fn get_burnchain_tokens_spent_for_block(
        &self,
        id_bhh: &StacksBlockId,
        epoch: &StacksEpochId,
    ) -> Option<u128> {
        let tenure_id_bhh = get_first_block_in_tenure(self.deref(), id_bhh, Some(epoch));
        get_miner_column(
            self.deref().deref(),
            &tenure_id_bhh,
            "burnchain_sortition_burn",
            |r| u64::from_row(r).expect("FATAL: malformed sortition burn"),
        )
        .map(|x| x.into())
    }

    fn get_burnchain_tokens_spent_for_winning_block(
        &self,
        id_bhh: &StacksBlockId,
        epoch: &StacksEpochId,
    ) -> Option<u128> {
        let tenure_id_bhh = get_first_block_in_tenure(self.deref(), id_bhh, Some(epoch));
        get_miner_column(
            self.deref().deref(),
            &tenure_id_bhh,
            "burnchain_commit_burn",
            |r| u64::from_row(r).expect("FATAL: malformed commit burn"),
        )
        .map(|x| x.into())
    }

    fn get_tokens_earned_for_block(
        &self,
        id_bhh: &StacksBlockId,
        epoch: &StacksEpochId,
    ) -> Option<u128> {
        let tenure_id_bhh = get_first_block_in_tenure(self.deref(), id_bhh, Some(epoch));
        get_matured_reward(self.deref(), &tenure_id_bhh, epoch).map(|x| x.total())
    }

    fn get_stacks_height_for_tenure_height(
        &self,
        tip: &StacksBlockId,
        tenure_height: u32,
    ) -> Option<u32> {
        let tenure_block_id = GetTenureStartId::get_tenure_block_id_at_cb_height(
            self.deref(),
            tip,
            tenure_height.into(),
        )
        .expect("FATAL: bad DB data for tenure height lookups")?;
        get_stacks_header_column(self.deref(), &tenure_block_id, "block_height", |r| {
            u64::from_row(r)
                .expect("FATAL: malformed block_height")
                .try_into()
                .expect("FATAL: blockchain too long")
        })
    }
}

impl HeadersDB for MARF<StacksBlockId> {
    fn get_stacks_block_header_hash_for_block(
        &self,
        id_bhh: &StacksBlockId,
        epoch: &StacksEpochId,
    ) -> Option<BlockHeaderHash> {
        get_stacks_header_column_from_table(
            self.sqlite_conn(),
            id_bhh,
            "block_hash",
            &|r| {
                BlockHeaderHash::from_column(r, "block_hash").expect("FATAL: malformed block hash")
            },
            epoch.uses_nakamoto_blocks(),
        )
    }

    fn get_burn_header_hash_for_block(
        &self,
        id_bhh: &StacksBlockId,
    ) -> Option<BurnchainHeaderHash> {
        get_stacks_header_column(self.sqlite_conn(), id_bhh, "burn_header_hash", |r| {
            BurnchainHeaderHash::from_row(r).expect("FATAL: malformed burn_header_hash")
        })
    }

    fn get_consensus_hash_for_block(
        &self,
        id_bhh: &StacksBlockId,
        epoch: &StacksEpochId,
    ) -> Option<ConsensusHash> {
        get_stacks_header_column_from_table(
            self.sqlite_conn(),
            id_bhh,
            "consensus_hash",
            &|r| ConsensusHash::from_row(r).expect("FATAL: malformed consensus_hash"),
            epoch.uses_nakamoto_blocks(),
        )
    }

    fn get_burn_block_time_for_block(
        &self,
        id_bhh: &StacksBlockId,
        epoch_opt: Option<&StacksEpochId>,
    ) -> Option<u64> {
        if let Some(epoch) = epoch_opt {
            get_stacks_header_column_from_table(
                self.sqlite_conn(),
                id_bhh,
                "burn_header_timestamp",
                &|r| u64::from_row(r).expect("FATAL: malformed burn_header_timestamp"),
                epoch.uses_nakamoto_blocks(),
            )
        } else {
            get_stacks_header_column(self.sqlite_conn(), id_bhh, "burn_header_timestamp", |r| {
                u64::from_row(r).expect("FATAL: malformed burn_header_timestamp")
            })
        }
    }

    fn get_stacks_block_time_for_block(&self, id_bhh: &StacksBlockId) -> Option<u64> {
        get_stacks_header_column_from_table(
            self.sqlite_conn(),
            id_bhh,
            "timestamp",
            &|r| u64::from_row(r).expect("FATAL: malformed timestamp"),
            true,
        )
    }

    fn get_burn_block_height_for_block(&self, id_bhh: &StacksBlockId) -> Option<u32> {
        get_stacks_header_column(self.sqlite_conn(), id_bhh, "burn_header_height", |r| {
            u64::from_row(r)
                .expect("FATAL: malformed burn_header_height")
                .try_into()
                .expect("FATAL: blockchain too long")
        })
    }

    fn get_vrf_seed_for_block(
        &self,
        id_bhh: &StacksBlockId,
        epoch: &StacksEpochId,
    ) -> Option<VRFSeed> {
        let tenure_id_bhh = get_first_block_in_tenure(self, id_bhh, Some(epoch));
        let (column_name, nakamoto) = if epoch.uses_nakamoto_blocks() {
            ("vrf_proof", true)
        } else {
            ("proof", false)
        };
        get_stacks_header_column_from_table(
            self.sqlite_conn(),
            &tenure_id_bhh.0,
            column_name,
            &|r| {
                let proof = VRFProof::from_column(r, column_name).expect("FATAL: malformed proof");
                VRFSeed::from_proof(&proof)
            },
            nakamoto,
        )
    }

    fn get_miner_address(
        &self,
        id_bhh: &StacksBlockId,
        epoch: &StacksEpochId,
    ) -> Option<StacksAddress> {
        let tenure_id_bhh = get_first_block_in_tenure(self, id_bhh, Some(epoch));
        get_miner_column(self.sqlite_conn(), &tenure_id_bhh, "address", |r| {
            let s: String = r.get_unwrap("address");
            let addr = StacksAddress::from_string(&s).expect("FATAL: malformed address");
            addr
        })
    }

    fn get_burnchain_tokens_spent_for_block(
        &self,
        id_bhh: &StacksBlockId,
        epoch: &StacksEpochId,
    ) -> Option<u128> {
        let tenure_id_bhh = get_first_block_in_tenure(self, id_bhh, Some(epoch));
        get_miner_column(
            self.sqlite_conn(),
            &tenure_id_bhh,
            "burnchain_sortition_burn",
            |r| u64::from_row(r).expect("FATAL: malformed sortition burn"),
        )
        .map(|x| x.into())
    }

    fn get_burnchain_tokens_spent_for_winning_block(
        &self,
        id_bhh: &StacksBlockId,
        epoch: &StacksEpochId,
    ) -> Option<u128> {
        let tenure_id_bhh = get_first_block_in_tenure(self, id_bhh, Some(epoch));
        get_miner_column(
            self.sqlite_conn(),
            &tenure_id_bhh,
            "burnchain_commit_burn",
            |r| u64::from_row(r).expect("FATAL: malformed commit burn"),
        )
        .map(|x| x.into())
    }

    fn get_tokens_earned_for_block(
        &self,
        id_bhh: &StacksBlockId,
        epoch: &StacksEpochId,
    ) -> Option<u128> {
        let tenure_id_bhh = get_first_block_in_tenure(self, id_bhh, Some(epoch));
        get_matured_reward(self, &tenure_id_bhh, epoch).map(|x| x.total())
    }

    fn get_stacks_height_for_tenure_height(
        &self,
        tip: &StacksBlockId,
        tenure_height: u32,
    ) -> Option<u32> {
        let tenure_block_id =
            GetTenureStartId::get_tenure_block_id_at_cb_height(self, tip, tenure_height.into())
                .expect("FATAL: bad DB data for tenure height lookups")?;
        get_stacks_header_column(self.sqlite_conn(), &tenure_block_id, "block_height", |r| {
            u64::from_row(r)
                .expect("FATAL: malformed block_height")
                .try_into()
                .expect("FATAL: blockchain too long")
        })
    }
}

/// Select a specific column from the headers table, specifying whether to use
/// the original block headers table or the Nakamoto block headers table.
pub fn get_stacks_header_column_from_table<F, R>(
    conn: &DBConn,
    id_bhh: &StacksBlockId,
    column_name: &str,
    loader: &F,
    nakamoto: bool,
) -> Option<R>
where
    F: Fn(&Row) -> R,
{
    let args = params![id_bhh];
    let table_name = if nakamoto {
        "nakamoto_block_headers"
    } else {
        "block_headers"
    };

    conn.query_row(
        &format!("SELECT {column_name} FROM {table_name} WHERE index_block_hash = ?",),
        args,
        |x| Ok(loader(x)),
    )
    .optional()
    .unwrap_or_else(|_| {
        panic!(
            "Unexpected SQL failure querying block header table for '{}'",
            column_name
        )
    })
}

fn get_stacks_header_column<F, R>(
    conn: &DBConn,
    id_bhh: &StacksBlockId,
    column_name: &str,
    loader: F,
) -> Option<R>
where
    F: Fn(&Row) -> R,
{
    match get_stacks_header_column_from_table(conn, id_bhh, column_name, &loader, false) {
        Some(x) => Some(x),
        None => get_stacks_header_column_from_table(conn, id_bhh, column_name, &loader, true),
    }
}

fn get_first_block_in_tenure<GTS: GetTenureStartId>(
    conn: &GTS,
    id_bhh: &StacksBlockId,
    epoch_opt: Option<&StacksEpochId>,
) -> TenureBlockId {
    let consensus_hash = match epoch_opt {
        Some(epoch) => {
            if !epoch.uses_nakamoto_blocks() {
                return id_bhh.clone().into();
            } else {
                get_stacks_header_column_from_table(
                    conn.conn(),
                    id_bhh,
                    "consensus_hash",
                    &|r| ConsensusHash::from_row(r).expect("FATAL: malformed consensus_hash"),
                    true,
                )
            }
        }
        None => {
            if get_stacks_header_column_from_table(
                conn.conn(),
                id_bhh,
                "consensus_hash",
                &|r| ConsensusHash::from_row(r).expect("FATAL: malformed consensus_hash"),
                false,
            )
            .is_some()
            {
                return id_bhh.clone().into();
            } else {
                get_stacks_header_column_from_table(
                    conn.conn(),
                    id_bhh,
                    "consensus_hash",
                    &|r| ConsensusHash::from_row(r).expect("FATAL: malformed consensus_hash"),
                    true,
                )
            }
        }
    };

    // SAFETY: if we reach this point, then `id_bhh` is a Nakamoto block and has a well-defined
    // tenure-start block ID.
    let ch = consensus_hash
        .expect("Unexpected SQL failure querying block header table for 'consensus_hash'");

    let tenure_start_id: TenureBlockId = conn
        .get_tenure_block_id(id_bhh, &ch)
        .expect("FATAL: failed to query DB for tenure-start block")
        .expect("FATAL: no tenure start block for Nakamoto block");

    tenure_start_id
}

fn get_miner_column<F, R>(
    conn: &DBConn,
    id_bhh: &TenureBlockId,
    column_name: &str,
    loader: F,
) -> Option<R>
where
    F: FnOnce(&Row) -> R,
{
    let args = params![id_bhh.0];
    conn.query_row(
        &format!(
            "SELECT {} FROM payments WHERE index_block_hash = ? AND miner = 1",
            column_name
        ),
        args,
        |x| Ok(loader(x)),
    )
    .optional()
    .unwrap_or_else(|_| {
        panic!(
            "Unexpected SQL failure querying miner payment table for '{}'",
            column_name
        )
    })
}

fn get_matured_reward<GTS: GetTenureStartId>(
    conn: &GTS,
    child_id_bhh: &TenureBlockId,
    epoch: &StacksEpochId,
) -> Option<MinerReward> {
    let table_name = if epoch.uses_nakamoto_blocks() {
        "nakamoto_block_headers"
    } else {
        "block_headers"
    };
    let parent_id_bhh = conn
        .conn()
        .query_row(
            &format!("SELECT parent_block_id FROM {table_name} WHERE index_block_hash = ?"),
            params![child_id_bhh.0],
            |x| {
                Ok(StacksBlockId::from_column(x, "parent_block_id")
                    .expect("Bad parent_block_id in database"))
            },
        )
        .optional()
        .expect("Unexpected SQL failure querying parent block ID");

    if let Some(parent_id_bhh) = parent_id_bhh {
        let parent_tenure_id = get_first_block_in_tenure(conn, &parent_id_bhh, None);
        StacksChainState::get_matured_miner_payment(conn.conn(), &parent_tenure_id, child_id_bhh)
            .expect("Unexpected SQL failure querying miner reward table")
    } else {
        None
    }
}

/// This trait describes SortitionDB connections. This is used
/// for methods that the chainstate needs to be in common between
/// different sortition db connections or handles, but that aren't
/// used by the `clarity_db` (and therefore shouldn't appear in BurnStateDB)
pub trait SortitionDBRef: BurnStateDB {
    fn get_pox_start_cycle_info(
        &self,
        sortition_id: &SortitionId,
        parent_stacks_block_burn_ht: u64,
        cycle_index: u64,
    ) -> Result<Option<PoxStartCycleInfo>, ChainstateError>;

    /// Return an upcasted dynamic reference for the sortition DB
    fn as_burn_state_db(&self) -> &dyn BurnStateDB;

    /// Return a pointer to the underlying sqlite connection or transaction for
    /// this DB reference
    fn sqlite_conn(&self) -> &Connection;
}

fn get_pox_start_cycle_info(
    handle: &mut SortitionHandleConn,
    parent_stacks_block_burn_ht: u64,
    cycle_index: u64,
) -> Result<Option<PoxStartCycleInfo>, ChainstateError> {
    let descended_from_last_pox_anchor = match handle.get_last_anchor_block_hash()? {
        Some(pox_anchor) => handle.descended_from(parent_stacks_block_burn_ht, &pox_anchor)?,
        None => return Ok(None),
    };

    if !descended_from_last_pox_anchor {
        return Ok(None);
    }

    let start_info = handle.get_reward_cycle_unlocks(cycle_index)?;
    debug!(
        "get_pox_start_cycle_info";
        "start_info" => ?start_info,
    );
    Ok(start_info)
}

impl SortitionDBRef for SortitionHandleTx<'_> {
    fn get_pox_start_cycle_info(
        &self,
        sortition_id: &SortitionId,
        parent_stacks_block_burn_ht: u64,
        cycle_index: u64,
    ) -> Result<Option<PoxStartCycleInfo>, ChainstateError> {
        let readonly_marf = self
            .index()
            .reopen_readonly()
            .expect("BUG: failure trying to get a read-only interface into the sortition db.");
        let mut context = self.context.clone();
        context.chain_tip = sortition_id.clone();
        let mut handle = SortitionHandleConn::new(&readonly_marf, context);

        get_pox_start_cycle_info(&mut handle, parent_stacks_block_burn_ht, cycle_index)
    }

    fn as_burn_state_db(&self) -> &dyn BurnStateDB {
        self
    }

    fn sqlite_conn(&self) -> &Connection {
        self.tx()
    }
}

impl SortitionDBRef for SortitionHandleConn<'_> {
    fn get_pox_start_cycle_info(
        &self,
        sortition_id: &SortitionId,
        parent_stacks_block_burn_ht: u64,
        cycle_index: u64,
    ) -> Result<Option<PoxStartCycleInfo>, ChainstateError> {
        let readonly_marf = self.index.reopen_readonly()?;
        let mut context = self.context.clone();
        context.chain_tip = sortition_id.clone();
        let mut handle = SortitionHandleConn::new(&readonly_marf, context);

        get_pox_start_cycle_info(&mut handle, parent_stacks_block_burn_ht, cycle_index)
    }

    fn as_burn_state_db(&self) -> &dyn BurnStateDB {
        self
    }

    fn sqlite_conn(&self) -> &Connection {
        self.conn()
    }
}

impl BurnStateDB for SortitionHandleTx<'_> {
    fn get_tip_burn_block_height(&self) -> Option<u32> {
        self.get_burn_block_height(&self.context.chain_tip)
    }

    fn get_tip_sortition_id(&self) -> Option<SortitionId> {
        Some(self.context.chain_tip.clone())
    }

    fn get_burn_block_height(&self, sortition_id: &SortitionId) -> Option<u32> {
        match SortitionDB::get_block_snapshot(self.tx(), sortition_id) {
            Ok(Some(x)) => Some(x.block_height as u32),
            _ => return None,
        }
    }

    /// Returns Some if `0 <= height < get_burn_block_height(sorition_id)`, and None otherwise.
    fn get_burn_header_hash(
        &self,
        height: u32,
        sortition_id: &SortitionId,
    ) -> Option<BurnchainHeaderHash> {
        let readonly_marf = self
            .index()
            .reopen_readonly()
            .expect("BUG: failure trying to get a read-only interface into the sortition db.");
        let mut context = self.context.clone();
        context.chain_tip = sortition_id.clone();
        let db_handle = SortitionHandleConn::new(&readonly_marf, context);
        match db_handle.get_block_snapshot_by_height(height as u64) {
            Ok(Some(x)) => Some(x.burn_header_hash),
            _ => return None,
        }
    }

    fn get_burn_block_height_for_header_hash(
        &self,
        burn_header_hash: &BurnchainHeaderHash,
        sortition_id: &SortitionId,
    ) -> Option<u32> {
        let readonly_marf = self
            .index()
            .reopen_readonly()
            .expect("BUG: failure trying to get a read-only interface into the sortition db.");
        let mut context = self.context.clone();
        context.chain_tip = sortition_id.clone();
        let db_handle = SortitionHandleConn::new(&readonly_marf, context);
        match db_handle.get_block_snapshot(burn_header_hash) {
            Ok(Some(snapshot)) => snapshot.block_height.try_into().ok(),
            _ => None,
        }
    }

    fn get_sortition_id_from_consensus_hash(
        &self,
        consensus_hash: &ConsensusHash,
    ) -> Option<SortitionId> {
        match SortitionDB::get_block_snapshot_consensus(self.tx(), consensus_hash) {
            Ok(Some(x)) => Some(x.sortition_id),
            _ => return None,
        }
    }

    fn get_stacks_epoch(&self, height: u32) -> Option<StacksEpoch> {
        SortitionDB::get_stacks_epoch(self.tx(), height as u64)
            .expect("BUG: failed to get epoch for burn block height")
    }

    fn get_burn_start_height(&self) -> u32 {
        self.context.first_block_height as u32
    }

    fn get_v1_unlock_height(&self) -> u32 {
        self.context.pox_constants.v1_unlock_height
    }

    fn get_v2_unlock_height(&self) -> u32 {
        self.context.pox_constants.v2_unlock_height
    }

    fn get_v3_unlock_height(&self) -> u32 {
        self.context.pox_constants.v3_unlock_height
    }

    fn get_pox_3_activation_height(&self) -> u32 {
        self.context.pox_constants.pox_3_activation_height
    }

    fn get_pox_4_activation_height(&self) -> u32 {
        self.context.pox_constants.pox_4_activation_height
    }

    fn get_pox_prepare_length(&self) -> u32 {
        self.context.pox_constants.prepare_length
    }

    fn get_pox_reward_cycle_length(&self) -> u32 {
        self.context.pox_constants.reward_cycle_length
    }

    fn get_pox_rejection_fraction(&self) -> u64 {
        self.context.pox_constants.pox_rejection_fraction
    }
    fn get_stacks_epoch_by_epoch_id(&self, epoch_id: &StacksEpochId) -> Option<StacksEpoch> {
        SortitionDB::get_stacks_epoch_by_epoch_id(self.tx(), epoch_id)
            .expect("BUG: failed to get epoch for epoch id")
    }
    fn get_pox_payout_addrs(
        &self,
        height: u32,
        sortition_id: &SortitionId,
    ) -> Option<(Vec<TupleData>, u128)> {
        let readonly_marf = self
            .index()
            .reopen_readonly()
            .expect("BUG: failure trying to get a read-only interface into the sortition db.");
        let mut context = self.context.clone();
        context.chain_tip = sortition_id.clone();
        let db_handle = SortitionHandleConn::new(&readonly_marf, context);

        let get_from = match get_ancestor_sort_id(&db_handle, height.into(), sortition_id)
            .expect("FATAL: failed to query sortition DB")
        {
            Some(sort_id) => sort_id,
            None => {
                return None;
            }
        };

        let (pox_addrs, payout) = self
            .get_reward_set_payouts_at(&get_from)
            .expect("FATAL: failed to query payouts");

        let addrs = pox_addrs
            .into_iter()
            .map(|addr| {
                addr.as_clarity_tuple()
                    .expect("FATAL: sortition DB did not store hash mode for PoX address")
            })
            .collect();

        Some((addrs, payout))
    }
}

impl BurnStateDB for SortitionHandleConn<'_> {
    fn get_tip_burn_block_height(&self) -> Option<u32> {
        let tip = match SortitionDB::get_block_snapshot(self.conn(), &self.context.chain_tip) {
            Ok(Some(x)) => x,
            _ => return None,
        };
        tip.block_height.try_into().ok()
    }

    fn get_tip_sortition_id(&self) -> Option<SortitionId> {
        let tip = match SortitionDB::get_block_snapshot(self.conn(), &self.context.chain_tip) {
            Ok(Some(x)) => x,
            _ => return None,
        };
        Some(tip.sortition_id)
    }

    fn get_burn_block_height(&self, sortition_id: &SortitionId) -> Option<u32> {
        match SortitionDB::get_block_snapshot(self.conn(), sortition_id) {
            Ok(Some(x)) => Some(x.block_height as u32),
            _ => return None,
        }
    }

    fn get_burn_header_hash(
        &self,
        height: u32,
        sortition_id: &SortitionId,
    ) -> Option<BurnchainHeaderHash> {
        let current_height = match self.get_burn_block_height(sortition_id) {
            None => {
                return None;
            }
            Some(height) => height,
        };

        if height > current_height {
            return None;
        }

        match self.get_block_snapshot_by_height(height as u64) {
            Ok(Some(x)) => Some(x.burn_header_hash),
            _ => return None,
        }
    }

    fn get_burn_block_height_for_header_hash(
        &self,
        burn_header_hash: &BurnchainHeaderHash,
        sortition_id: &SortitionId,
    ) -> Option<u32> {
        let readonly_marf = self.index.reopen_readonly().ok()?;
        let mut context = self.context.clone();
        context.chain_tip = sortition_id.clone();
        let db_handle = SortitionHandleConn::new(&readonly_marf, context);
        match db_handle.get_block_snapshot(burn_header_hash) {
            Ok(Some(snapshot)) => snapshot.block_height.try_into().ok(),
            _ => None,
        }
    }

    fn get_sortition_id_from_consensus_hash(
        &self,
        consensus_hash: &ConsensusHash,
    ) -> Option<SortitionId> {
        match SortitionDB::get_block_snapshot_consensus(self.conn(), consensus_hash) {
            Ok(Some(x)) => Some(x.sortition_id),
            _ => return None,
        }
    }

    fn get_stacks_epoch(&self, height: u32) -> Option<StacksEpoch> {
        SortitionDB::get_stacks_epoch(self.conn(), height as u64)
            .expect("BUG: failed to get epoch for burn block height")
    }

    fn get_burn_start_height(&self) -> u32 {
        self.context.first_block_height as u32
    }

    fn get_v1_unlock_height(&self) -> u32 {
        self.context.pox_constants.v1_unlock_height
    }

    fn get_v2_unlock_height(&self) -> u32 {
        self.context.pox_constants.v2_unlock_height
    }

    fn get_v3_unlock_height(&self) -> u32 {
        self.context.pox_constants.v3_unlock_height
    }

    fn get_pox_3_activation_height(&self) -> u32 {
        self.context.pox_constants.pox_3_activation_height
    }

    fn get_pox_4_activation_height(&self) -> u32 {
        self.context.pox_constants.pox_4_activation_height
    }

    fn get_pox_prepare_length(&self) -> u32 {
        self.context.pox_constants.prepare_length
    }

    fn get_pox_reward_cycle_length(&self) -> u32 {
        self.context.pox_constants.reward_cycle_length
    }

    fn get_pox_rejection_fraction(&self) -> u64 {
        self.context.pox_constants.pox_rejection_fraction
    }
    fn get_stacks_epoch_by_epoch_id(&self, epoch_id: &StacksEpochId) -> Option<StacksEpoch> {
        SortitionDB::get_stacks_epoch_by_epoch_id(self.conn(), epoch_id)
            .expect("BUG: failed to get epoch for epoch id")
    }
    fn get_pox_payout_addrs(
        &self,
        height: u32,
        sortition_id: &SortitionId,
    ) -> Option<(Vec<TupleData>, u128)> {
        let get_from = match get_ancestor_sort_id(self, height.into(), sortition_id)
            .expect("FATAL: failed to query sortition DB")
        {
            Some(sort_id) => sort_id,
            None => {
                return None;
            }
        };

        let (pox_addrs, payout) = self
            .get_reward_set_payouts_at(&get_from)
            .expect("FATAL: failed to query payouts");

        let addrs = pox_addrs
            .into_iter()
            .map(|addr| {
                addr.as_clarity_tuple()
                    .expect("FATAL: sortition DB did not store hash mode for PoX address")
            })
            .collect();

        Some((addrs, payout))
    }
}

pub struct MemoryBackingStore {
    side_store: Connection,
}

impl MemoryBackingStore {
    pub fn new() -> MemoryBackingStore {
        let side_store = SqliteConnection::memory().unwrap();

        let mut memory_marf = MemoryBackingStore { side_store };

        memory_marf.as_clarity_db().initialize();

        memory_marf
    }

    pub fn as_clarity_db(&mut self) -> ClarityDatabase<'_> {
        ClarityDatabase::new(self, &NULL_HEADER_DB, &NULL_BURN_STATE_DB)
    }

    /// Returns a new ClarityDatabase with underlying databases `headers_db` and
    /// `burn_state_db`.
    pub fn as_clarity_db_with_databases<'a>(
        &'a mut self,
        headers_db: &'a dyn HeadersDB,
        burn_state_db: &'a dyn BurnStateDB,
    ) -> ClarityDatabase<'a> {
        ClarityDatabase::new(self, headers_db, burn_state_db)
    }

    pub fn as_analysis_db(&mut self) -> AnalysisDatabase<'_> {
        AnalysisDatabase::new(self)
    }
}

impl ClarityBackingStore for MemoryBackingStore {
    fn set_block_hash(&mut self, bhh: StacksBlockId) -> Result<StacksBlockId, VmExecutionError> {
        Err(RuntimeError::UnknownBlockHeaderHash(BlockHeaderHash(bhh.0)).into())
    }

    fn get_data(&mut self, key: &str) -> Result<Option<String>, VmExecutionError> {
        SqliteConnection::get(self.get_side_store(), key)
    }

    fn get_data_from_path(&mut self, hash: &TrieHash) -> Result<Option<String>, VmExecutionError> {
        SqliteConnection::get(self.get_side_store(), hash.to_string().as_str())
    }

    fn get_data_with_proof(
        &mut self,
        key: &str,
    ) -> Result<Option<(String, Vec<u8>)>, VmExecutionError> {
        Ok(SqliteConnection::get(self.get_side_store(), key)?.map(|x| (x, vec![])))
    }

    fn get_data_with_proof_from_path(
        &mut self,
        key: &TrieHash,
    ) -> Result<Option<(String, Vec<u8>)>, VmExecutionError> {
        Ok(
            SqliteConnection::get(self.get_side_store(), key.to_string().as_str())?
                .map(|x| (x, vec![])),
        )
    }

    fn get_side_store(&mut self) -> &Connection {
        &self.side_store
    }

    fn get_block_at_height(&mut self, height: u32) -> Option<StacksBlockId> {
        if height == 0 {
            Some(StacksBlockId::sentinel())
        } else {
            None
        }
    }

    fn get_open_chain_tip(&mut self) -> StacksBlockId {
        StacksBlockId::sentinel()
    }

    fn get_open_chain_tip_height(&mut self) -> u32 {
        0
    }

    fn get_current_block_height(&mut self) -> u32 {
        0
    }

    fn get_cc_special_cases_handler(&self) -> Option<SpecialCaseHandler> {
        Some(&handle_contract_call_special_cases)
    }

    fn put_all_data(&mut self, items: Vec<(String, String)>) -> Result<(), VmExecutionError> {
        for (key, value) in items.into_iter() {
            SqliteConnection::put(self.get_side_store(), &key, &value)?;
        }
        Ok(())
    }

    fn get_contract_hash(
        &mut self,
        contract: &QualifiedContractIdentifier,
    ) -> Result<(StacksBlockId, Sha512Trunc256Sum), VmExecutionError> {
        sqlite_get_contract_hash(self, contract)
    }

    fn insert_metadata(
        &mut self,
        contract: &QualifiedContractIdentifier,
        key: &str,
        value: &str,
    ) -> Result<(), VmExecutionError> {
        sqlite_insert_metadata(self, contract, key, value)
    }

    fn get_metadata(
        &mut self,
        contract: &QualifiedContractIdentifier,
        key: &str,
    ) -> Result<Option<String>, VmExecutionError> {
        sqlite_get_metadata(self, contract, key)
    }

    fn get_metadata_manual(
        &mut self,
        at_height: u32,
        contract: &QualifiedContractIdentifier,
        key: &str,
    ) -> Result<Option<String>, VmExecutionError> {
        sqlite_get_metadata_manual(self, at_height, contract, key)
    }
}
