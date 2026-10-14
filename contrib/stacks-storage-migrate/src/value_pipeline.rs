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

//! Byte-bounded source decoding with ordered, independently verified worker results.

use std::thread;

use blockstack_lib::chainstate::stacks::index::MARFValue;
use blockstack_lib::chainstate::stacks::index::inline_value::InlineValue;
use blockstack_lib::clarity_vm::database::binary_value_store::{self, ValueStorageFormat};
use blockstack_lib::clarity_vm::database::value_extents::InlineValueRecord;
use rusqlite::Row;
use stable_value_format::{MAX_DESCRIPTOR_BYTES, MAX_RECORD_BYTES};
use stacks_common::util::hash::hex_bytes;

use crate::Result;

/// Source representation retained only for one bounded batch.
enum Body {
    /// Upstream canonical text, including hex-encoded serialized values.
    Text(String),
    /// A source-only adapter for already packed SQL values.
    Packed(Vec<u8>, Vec<u8>),
}

/// One independently decodable SQL row.
pub struct Input {
    /// Exact source commitment.
    commitment: MARFValue,
    /// Owned bytes released after this batch.
    body: Body,
}

/// Verified final representation, emitted in original row order.
#[derive(Debug, PartialEq)]
pub struct Prepared {
    /// Exact logical commitment, verified before any destination write.
    pub commitment: MARFValue,
    /// Final packed bytes.
    pub payload: Vec<u8>,
    /// Exact reconstruction descriptor.
    pub descriptor: Vec<u8>,
    /// Whether these bytes fit the canonical inline-leaf policy.
    pub inline: bool,
}

impl Input {
    /// Borrow/check SQL lengths before allocating one legal source record.
    pub fn read(row: &Row<'_>, format: ValueStorageFormat) -> Result<Self> {
        let (commitment, body) = match format {
            ValueStorageFormat::LegacyText => {
                let hash = hex_bytes(row.get_ref(0)?.as_str()?)?;
                let commitment = MARFValue(
                    hash.try_into()
                        .map_err(|_| "Clarity key must be 40 bytes")?,
                );
                let text = row.get_ref(1)?.as_str()?;
                if text.len() > MAX_RECORD_BYTES as usize * 2 {
                    return Err("legacy value exceeds canonical record bound".into());
                }
                (commitment, Body::Text(text.to_owned()))
            }
            ValueStorageFormat::BinaryV1 => {
                let commitment = MARFValue(
                    row.get_ref(0)?
                        .as_blob()?
                        .try_into()
                        .map_err(|_| "Clarity key must be 40 bytes")?,
                );
                let payload = row.get_ref(1)?.as_blob()?;
                let descriptor = row.get_ref(2)?.as_blob_or_null()?.unwrap_or_default();
                if payload.len() > MAX_RECORD_BYTES as usize
                    || descriptor.len() > MAX_DESCRIPTOR_BYTES as usize
                {
                    return Err("packed source value exceeds canonical record bound".into());
                }
                (
                    commitment,
                    Body::Packed(payload.to_vec(), descriptor.to_vec()),
                )
            }
        };
        Ok(Self { commitment, body })
    }

    /// Owned source payload bytes charged against the batch budget.
    pub fn bytes(&self) -> usize {
        match &self.body {
            Body::Text(text) => text.len(),
            Body::Packed(payload, descriptor) => payload.len() + descriptor.len(),
        }
    }

    /// Verify the commitment and final inline reconstruction independently of SQLite.
    fn prepare(&self) -> Result<Prepared> {
        let (canonical, payload, descriptor) = match &self.body {
            Body::Text(text) => {
                if self.commitment != MARFValue::from_value(text) {
                    return Err("legacy Clarity value commitment mismatch".into());
                }
                let encoded = binary_value_store::encode_migrated(text)?;
                (
                    text.clone(),
                    encoded.record().to_vec(),
                    encoded.shape().unwrap_or_default().to_vec(),
                )
            }
            Body::Packed(payload, descriptor) => {
                let canonical = binary_value_store::audit_stored_record(
                    &self.commitment,
                    payload,
                    if descriptor.is_empty() {
                        None
                    } else {
                        Some(descriptor)
                    },
                )?;
                (canonical, payload.clone(), descriptor.clone())
            }
        };
        let inline = InlineValue::fits_inline(payload.len(), descriptor.len());
        if inline {
            let leaf = InlineValue::from_parts(&payload, &descriptor)?;
            if InlineValueRecord::from_inline(&leaf).canonical()? != canonical {
                return Err("inline reconstruction changed during conversion".into());
            }
        }
        Ok(Prepared {
            commitment: self.commitment.clone(),
            payload,
            descriptor,
            inline,
        })
    }
}

/// Prepare a bounded batch with stable output order and no destination-side effects.
pub fn prepare(inputs: &[Input], workers: usize) -> Result<Vec<Prepared>> {
    if inputs.is_empty() {
        return Ok(Vec::new());
    }
    let count = workers.max(1).min(inputs.len());
    let result = thread::scope(|scope| {
        let handles: Vec<_> = inputs
            .chunks(inputs.len().div_ceil(count))
            .map(|chunk| {
                scope.spawn(move || {
                    chunk
                        .iter()
                        .map(|value| value.prepare().map_err(|error| error.to_string()))
                        .collect::<std::result::Result<Vec<_>, String>>()
                })
            })
            .collect();
        let mut prepared = Vec::with_capacity(inputs.len());
        for handle in handles {
            prepared.extend(
                handle
                    .join()
                    .map_err(|_| "source value worker panicked".to_owned())??,
            );
        }
        Ok::<_, String>(prepared)
    });
    result.map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Worker count cannot change the bytes, order, or rejection of a malformed commitment.
    #[test]
    fn ordered_workers_match_serial_and_reject_corruption() {
        let mut values: Vec<_> = (0..513)
            .map(|n| {
                let text = format!("opaque-{n}-{}", "x".repeat(n % 64));
                Input {
                    commitment: MARFValue::from_value(&text),
                    body: Body::Text(text),
                }
            })
            .collect();
        assert_eq!(prepare(&values, 1).unwrap(), prepare(&values, 4).unwrap());
        values[300].commitment = MARFValue([7; 40]);
        assert!(prepare(&values, 1).is_err());
        assert!(prepare(&values, 4).is_err());
    }
}
