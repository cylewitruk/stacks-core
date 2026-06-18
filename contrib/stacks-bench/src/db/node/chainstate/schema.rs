use diesel::prelude::*;

table! {
    db_config (version) {
        version -> Integer,
        mainnet -> Bool,
        chain_id -> Integer
    }
}

table! {
    block_headers (consensus_hash, block_hash) {
        consensus_hash -> Text,
        block_hash -> Text,
        index_block_hash -> Text,
        parent_block_id -> Text,
        block_height -> BigInt,
        burn_header_hash -> Text,
        burn_header_height -> BigInt,
    }
}

table! {
    nakamoto_block_headers (consensus_hash, block_hash) {
        consensus_hash -> Text,
        block_hash -> Text,
        index_block_hash -> Text,
        parent_block_id -> Text,
        block_height -> BigInt,
        burn_header_hash -> Text,
        burn_header_height -> BigInt,
        header_type -> Text,
    }
}
