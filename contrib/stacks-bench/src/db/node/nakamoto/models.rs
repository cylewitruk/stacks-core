use diesel::prelude::*;

use super::schema::nakamoto_staging_blocks;

#[derive(Queryable, Selectable, Identifiable, Debug, Clone)]
#[diesel(table_name = nakamoto_staging_blocks)]
#[diesel(primary_key(block_hash, consensus_hash))]
pub struct NakamotoStagingBlock {
    pub block_hash: String,
    pub consensus_hash: String,
    pub parent_block_id: String,
    pub height: i32,
    pub index_block_hash: String,
    pub data: Vec<u8>,
}
