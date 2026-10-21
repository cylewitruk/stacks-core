use diesel::prelude::*;
use diesel::sql_types::Integer;
// Define a struct to map the PRAGMA result
#[derive(QueryableByName, Debug)]
pub struct CheckpointResult {
    #[diesel(sql_type = Integer)]
    #[diesel(column_name = "busy")]
    pub busy: i32,
    #[diesel(sql_type = Integer)]
    #[diesel(column_name = "log")]
    pub log: i32,
    #[diesel(sql_type = Integer)]
    #[diesel(column_name = "checkpointed")]
    pub checkpointed: i32,
}
