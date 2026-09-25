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

//! Offline entry point for migrating a Clarity side store to Binary V1.

mod cli;
mod output;

use std::error::Error;

use clap::Parser;
use cli::Cli;
use stackslib::clarity_vm::database::binary_value_store;

/// Parse operator input and drive the stackslib-owned migration engine.
fn main() -> Result<(), Box<dyn Error>> {
    let config = Cli::parse().into_config();
    output::print_configuration(&config);
    binary_value_store::migrate(&config, &mut output::print_event)?;
    Ok(())
}
