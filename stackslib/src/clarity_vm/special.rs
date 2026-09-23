// Copyright (C) 2013-2020 Blockstack PBC, a public benefit corporation
// Copyright (C) 2020 Stacks Open Internet Foundation
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

use clarity::boot_util::boot_code_id;
use clarity::vm::ValueRef;
use clarity::vm::contexts::GlobalContext;
use clarity::vm::errors::VmExecutionError;
use clarity::vm::types::{PrincipalData, QualifiedContractIdentifier, Value};
use pox_locking::{POX_1_NAME, POX_2_NAME, POX_3_NAME, POX_4_NAME, POX_5_NAME};

/// Handle special cases of contract-calls -- namely, those into PoX that should lock up STX
pub fn handle_contract_call_special_cases(
    global_context: &mut GlobalContext,
    sender: Option<&PrincipalData>,
    sponsor: Option<&PrincipalData>,
    contract_id: &QualifiedContractIdentifier,
    function_name: &str,
    args: &[Value],
    result: &Value,
) -> Result<(), VmExecutionError> {
    pox_locking::handle_contract_call_special_cases(
        global_context,
        sender,
        sponsor,
        contract_id,
        function_name,
        args,
        result,
    )
}

/// Only PoX special cases require compatibility-owned arguments and results.
pub fn handle_contract_call_special_cases_ref(
    global_context: &mut GlobalContext,
    sender: Option<&PrincipalData>,
    sponsor: Option<&PrincipalData>,
    contract_id: &QualifiedContractIdentifier,
    function_name: &str,
    args: &[ValueRef<'_>],
    result: &ValueRef<'_>,
) -> Result<(), VmExecutionError> {
    if ![POX_1_NAME, POX_2_NAME, POX_3_NAME, POX_4_NAME, POX_5_NAME]
        .iter()
        .any(|name| *contract_id == boot_code_id(name, global_context.mainnet))
    {
        return Ok(());
    }
    let args = args
        .iter()
        .map(|value| value.as_ref().clone())
        .collect::<Vec<_>>();
    handle_contract_call_special_cases(
        global_context,
        sender,
        sponsor,
        contract_id,
        function_name,
        &args,
        result.as_ref(),
    )
}
