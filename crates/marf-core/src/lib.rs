// Copyright (C) 2013-2020 Blockstack PBC, a public benefit corporation
// Copyright (C) 2020-2026 Stacks Open Internet Foundation
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

//! Neutral MARF engine with built-in SQLite/blob persistence.
use std::{error, fmt, io};

/// Enter one statically selected logical diagnostic span.
macro_rules! observe {
    ($observer:ty, $operation:ident) => {{
        if <$observer as $crate::observe::Observer>::ENABLED
            && !<$observer as $crate::observe::Observer>::is_suppressed()
        {
            use std::sync::atomic::{AtomicU64, Ordering};
            static SAMPLES: AtomicU64 = AtomicU64::new(0);
            let operation = $crate::observe::Operation::$operation;
            let rate = u64::from(operation.sample_rate());
            let timed = rate == 1 || SAMPLES.fetch_add(1, Ordering::Relaxed).is_multiple_of(rate);
            Some(<$observer as $crate::observe::Observer>::enter(
                operation, timed,
            ))
        } else {
            None
        }
    }};
}

macro_rules! count {
    ($field:ident) => {
        #[cfg(feature = "counters")]
        $crate::stats::update(|c| c.$field += 1);
    };
    ($field:ident, $n:expr) => {
        #[cfg(feature = "counters")]
        $crate::stats::update(|c| c.$field += u64::from($n));
    };
}

pub mod access;
pub mod archive;
pub mod bytes;
pub mod codec;
pub mod hash;
pub mod marf;
pub mod node;
pub mod observe;
mod positioned_io;
pub mod sqlite;
pub mod stats;
pub mod storage;
pub mod trie;
mod value;

#[cfg(any(test, feature = "testing"))]
pub use marf::WalkProfile;
pub use marf::{MARF, MARFOpenOpts, MarfConnection, MarfTransaction};
pub use value::{
    MARF_VALUE_ENCODED_SIZE, MARFValue, SENTINEL_ARRAY, TRIEHASH_ENCODED_SIZE, TrieHash, TrieId,
    TrieId32,
};

#[derive(Debug)]
pub enum Error {
    NotOpenedError,
    IOError(io::Error),
    SQLError(rusqlite::Error),
    NotFoundError,
    ExistsError,
    CorruptionError(String),
    ReadOnlyError,
    UnconfirmedError,
    InProgressError,
    WriteNotBegunError,
    RestoreMarfBlockError(Box<Error>),
    NonMatchingForks([u8; 32], [u8; 32]),
    OverflowError,
    NodeTooDeep,
    /// Read at a block strictly below the prune height of a pruned MARF.
    HistoricalReadInPrunedRange {
        block_height: u32,
        prune_height: u32,
    },
    /// Operation is not supported on a pruned MARF (e.g. proof generation).
    UnsupportedOnPrunedMarf(&'static str),
    /// A destination path required to be empty already exists.
    DestinationExists(String),
}

impl From<io::Error> for Error {
    fn from(err: io::Error) -> Self {
        Error::IOError(err)
    }
}

impl From<rusqlite::Error> for Error {
    fn from(err: rusqlite::Error) -> Self {
        if let rusqlite::Error::QueryReturnedNoRows = err {
            Error::NotFoundError
        } else {
            Error::SQLError(err)
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            Error::IOError(ref e) => fmt::Display::fmt(e, f),
            Error::SQLError(ref e) => fmt::Display::fmt(e, f),
            Error::CorruptionError(ref s) => fmt::Display::fmt(s, f),
            Error::NotOpenedError => write!(f, "Tried to read data from unopened storage"),
            Error::NotFoundError => write!(f, "Object not found"),
            Error::ExistsError => write!(f, "Object exists"),
            Error::ReadOnlyError => write!(f, "Storage is in read-only mode"),
            Error::UnconfirmedError => write!(f, "Storage is in unconfirmed mode"),
            Error::InProgressError => write!(f, "Write was in progress"),
            Error::WriteNotBegunError => write!(f, "Write has not begun"),
            Error::RestoreMarfBlockError(_) => write!(
                f,
                "Failed to restore previous open block during block header check"
            ),
            Error::NonMatchingForks(_, _) => {
                write!(f, "The supplied blocks are not in the same fork")
            }
            Error::OverflowError => write!(f, "Overflow"),
            Error::NodeTooDeep => write!(f, "Node is too deeply buried under patches"),
            Error::HistoricalReadInPrunedRange {
                block_height,
                prune_height,
            } => write!(
                f,
                "Historical read at height {block_height} below prune height {prune_height} \
                 is not supported on a pruned MARF"
            ),
            Error::UnsupportedOnPrunedMarf(op) => {
                write!(f, "Operation `{op}` is not supported on a pruned MARF")
            }
            Error::DestinationExists(ref p) => {
                write!(f, "Destination path already exists: {p}")
            }
        }
    }
}

impl error::Error for Error {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        match *self {
            Error::IOError(ref e) => Some(e),
            Error::SQLError(ref e) => Some(e),
            Error::RestoreMarfBlockError(ref e) => Some(e),
            _ => None,
        }
    }
}

/// A convenience for `Error::CorruptionError(format!(..))`.
#[macro_export]
macro_rules! corrupt {
    ($($arg:tt)*) => { $crate::Error::CorruptionError(format!($($arg)*)) };
}
