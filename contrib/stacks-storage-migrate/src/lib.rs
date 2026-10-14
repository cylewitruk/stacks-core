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

//! Direct offline conversion from legacy MARFs to canonical storage.

mod ancestors;
mod chainstate;
mod clarity;
mod memberships;
mod migration;
mod ordered_pipeline;
mod preparation;
mod reused_values;
mod schema;
mod source;
mod space;
mod trie_pipeline;
mod value_lookup;
mod value_pipeline;

pub use chainstate::migrate_chainstate;
pub use migration::{Config, Outcome, migrate, migrate_reusing_clarity};
pub use preparation::{prepare_chainstate_in_place, prepare_marf_in_place, verify};

/// Errors from source validation, conversion, or publication.
pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
