//! Offline, version-gated V4-to-V4.1 MARF conversion using the shared migration pipeline.

#[path = "../../stacks-trie-format-v4/src/ancestor_cache.rs"]
mod ancestor_cache;
mod codec;
#[path = "../../stacks-trie-format-v4/src/migration.rs"]
mod migration;
#[path = "../../stacks-trie-format-v4/src/ordered_pipeline.rs"]
mod ordered_pipeline;
#[path = "../../stacks-trie-format-v4/src/relocation.rs"]
mod relocation;
#[path = "../../stacks-trie-format-v4/src/relocation_index.rs"]
mod relocation_index;

pub use migration::{Config, plan, rewrite};
