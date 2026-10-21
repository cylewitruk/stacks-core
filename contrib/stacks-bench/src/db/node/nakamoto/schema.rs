use diesel::prelude::*;

table! {
    nakamoto_staging_blocks (block_hash, consensus_hash) {
        block_hash -> Text,
        consensus_hash -> Text,
        parent_block_id -> Text,
        height -> Integer,
        index_block_hash -> Text,
        data -> Binary,
    }
}
