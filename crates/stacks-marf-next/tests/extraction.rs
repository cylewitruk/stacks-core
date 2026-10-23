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

//! Real package proof generation agrees with facade conveniences.
/// The extracted proof encoder must match the built-in convenience API.
#[test]
fn extracted_proofs_match_core_on_building_and_committed_tries() {
    use stacks_common::codec::StacksMessageCodec;
    use stacks_common::types::chainstate::{StacksBlockId, TrieHash};
    use stacks_marf_next::ClarityMarfTrieId;
    use stacks_marf_next::marf::{MARF, MARFOpenOpts, MarfConnection};
    use stacks_marf_next::proofs::Prove;
    let mut marf = MARF::<StacksBlockId>::from_path(":memory:", MARFOpenOpts::default()).unwrap();
    let tip = StacksBlockId([7; 32]);
    marf.begin(&StacksBlockId::sentinel(), &tip).unwrap();
    let value = stacks_marf_next::MARFValue([9; 40]);
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
