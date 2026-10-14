// Copyright (C) 2026 Stacks Open Internet Foundation
// SPDX-License-Identifier: GPL-3.0-or-later

//! Checked path-prefix access shared by the canonical mmap branch decoders.

use super::Error;

/// Decode a path prefix without examining the child directory.
pub fn path_prefix(bytes: &[u8]) -> Result<&[u8], Error> {
    let length = usize::from(
        *bytes
            .first()
            .ok_or_else(|| invalid("Missing path length"))?,
    );
    if length > 32 {
        return Err(invalid("Oversized node path"));
    }
    bytes
        .get(1..1 + length)
        .ok_or_else(|| invalid("Truncated node path"))
}

/// Describe invalid mapped bytes without accessing their child payload.
fn invalid(message: &str) -> Error {
    Error::CorruptionError(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every allowed path length is borrowed exactly; malformed prefixes fail closed.
    #[test]
    fn mapped_paths_validate_bounds() {
        for length in 0..=32u8 {
            let mut bytes = vec![length];
            bytes.extend(std::iter::repeat_n(7, usize::from(length)));
            assert_eq!(path_prefix(&bytes).unwrap(), &bytes[1..]);
            for end in 0..bytes.len() {
                assert!(path_prefix(&bytes[..end]).is_err());
            }
        }
        assert!(path_prefix(&[33; 34]).is_err());
    }
}
