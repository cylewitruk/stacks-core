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

//! Extraction guard: compile the real domains as part of a separate crate.
//! The domains and their portable I/O helper depend only on public core APIs.
//! Included domain unit tests deliberately run here too, so their backend setup
//! imports the concrete crate directly without broadening algorithm facades.
#![allow(dead_code)]
#[macro_use]
extern crate stacks_common;
#[macro_use]
extern crate serde_derive;
#[allow(unused_imports)]
#[macro_use(o, slog_log, slog_trace, slog_debug, slog_info, slog_warn, slog_error)]
extern crate slog;

pub use stacks_marf_next::{
    ClarityMarfTrieId, Error, MARFValue, MarfTrieId, access, archive, codec, corrupt, hash, node,
};
// These facades contain only configuration and format vocabulary. Backend
// types stay unavailable through the paths used by the included algorithms.
mod marf {
    pub use stacks_marf_next::marf::MARFOpenOpts;
}
mod trie {
    pub use stacks_marf_next::trie::{HEADER_LEN, ROOT_OFFSET};
}
#[path = "../src/positioned_io.rs"]
mod positioned_io;
#[path = "../src/proofs/mod.rs"]
pub mod proofs;
#[path = "../src/pruning/mod.rs"]
pub mod pruning;

/// The extracted proof encoder must match the built-in convenience API.
#[test]
fn extracted_proofs_match_core_on_building_and_committed_tries() {
    use proofs::Prove;
    use stacks_common::codec::StacksMessageCodec;
    use stacks_common::types::chainstate::{StacksBlockId, TrieHash};
    use stacks_marf_next::marf::{MARF, MARFOpenOpts, MarfConnection};
    let mut marf = MARF::<StacksBlockId>::from_path(":memory:", MARFOpenOpts::default()).unwrap();
    let tip = StacksBlockId([7; 32]);
    marf.begin(&StacksBlockId::sentinel(), &tip).unwrap();
    let value = MARFValue([9; 40]);
    marf.insert("test", value.clone()).unwrap();
    for committed in [false, true] {
        if committed {
            marf.commit().unwrap();
        }
        marf.with_conn(|view| {
            let extracted = view
                .prove(&TrieHash::from_key("test"), &value, &tip)
                .unwrap();
            let builtin = view.get_with_proof(&tip, "test").unwrap().unwrap().1;
            let mut actual = Vec::new();
            let mut expected = Vec::new();
            extracted.consensus_serialize(&mut actual).unwrap();
            builtin.consensus_serialize(&mut expected).unwrap();
            assert_eq!(actual, expected);
        });
    }
}
