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

//! Proof-domain failures preserve the concrete source's typed cause.
use std::{error, fmt};
/// Failure to construct a proof; verification itself needs no source.
#[derive(Debug)]
pub enum Error<E> {
    /// Failure from the authenticated source, without string conversion.
    Source(E),
    /// Requested path or value is absent.
    NotFoundError,
    /// The source violates an authenticated structure invariant.
    CorruptionError(String),
    /// The source has pruned information needed for a proof.
    UnsupportedOnPrunedMarf(&'static str),
}
impl<E> From<E> for Error<E> {
    fn from(e: E) -> Self {
        Self::Source(e)
    }
}
impl<E: fmt::Display> fmt::Display for Error<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Source(e) => fmt::Display::fmt(e, f),
            Self::NotFoundError => f.write_str("Object not found"),
            Self::CorruptionError(e) => f.write_str(e),
            Self::UnsupportedOnPrunedMarf(op) => {
                write!(f, "Operation `{op}` is not supported on a pruned MARF")
            }
        }
    }
}
impl<E: error::Error + 'static> error::Error for Error<E> {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        if let Self::Source(e) = self {
            Some(e)
        } else {
            None
        }
    }
}
