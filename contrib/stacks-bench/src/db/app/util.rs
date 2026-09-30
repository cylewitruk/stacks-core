use blockstack_lib::chainstate::stacks::{TransactionPayload, TransactionPayloadID};

use crate::db::app::models::StacksTxType;

fn tx_type_display_name(id: TransactionPayloadID) -> &'static str {
    use TransactionPayloadID::*;
    match id {
        TokenTransfer => "Token Transfer",
        SmartContract => "Contract Deploy",
        VersionedSmartContract => "Contract Deploy (Versioned)",
        ContractCall => "Contract Call",
        PoisonMicroblock => "Poison Microblock",
        Coinbase => "Coinbase",
        CoinbaseToAltRecipient => "Coinbase (Alt. Recipient)",
        NakamotoCoinbase => "Coinbase (Nakamoto)",
        TenureChange => "Tenure Change",
    }
}

pub fn resolve_tx_type(payload: &TransactionPayload) -> StacksTxType {
    let id = payload.payload_id();
    StacksTxType {
        id: id as i32,
        name: tx_type_display_name(id).to_string(),
    }
}
