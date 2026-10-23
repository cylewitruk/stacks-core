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

//! Typed failures from independent archive and staging capabilities.
use std::{error, fmt, io};
/// Failure to construct an archive without flattening its cause.
#[derive(Debug)]
pub enum Error<S, D> {
    /// Historical source failure.
    Source(S),
    /// Destination I/O/publication failure.
    Destination(D),
    /// Canonical node format failure.
    Format(marf_core::Error),
    /// Temporary-stage I/O failure.
    Io(io::Error),
    /// Required historical state is absent.
    NotFoundError,
    /// An archive invariant is violated.
    CorruptionError(String),
    /// A node count or offset is out of range.
    OverflowError,
}
impl<S, D> From<marf_core::Error> for Error<S, D> {
    fn from(e: marf_core::Error) -> Self {
        Self::Format(e)
    }
}
impl<S, D> From<io::Error> for Error<S, D> {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}
impl<S: fmt::Display, D: fmt::Display> fmt::Display for Error<S, D> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Source(e) => e.fmt(f),
            Self::Destination(e) => e.fmt(f),
            Self::Format(e) => e.fmt(f),
            Self::Io(e) => e.fmt(f),
            Self::NotFoundError => f.write_str("Object not found"),
            Self::CorruptionError(e) => f.write_str(e),
            Self::OverflowError => f.write_str("Overflow"),
        }
    }
}
impl<S: error::Error + 'static, D: error::Error + 'static> error::Error for Error<S, D> {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        match self {
            Self::Source(e) => Some(e),
            Self::Destination(e) => Some(e),
            Self::Format(e) => Some(e),
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}
