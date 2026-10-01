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

//! Explicit, verified reuse of a stopped converter's completed value extraction.

use std::fs::{self, File};
use std::path::{Component, Path};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;

use blockstack_lib::chainstate::stacks::index::MARFValue;
use blockstack_lib::clarity_vm::database::binary_value_store::{self, ValueStorageFormat};
use blockstack_lib::clarity_vm::database::value_extents::{StableMappedValueRecord, ValueBackend};
use blockstack_lib::util_lib::db::sqlite_readonly_uri;
use rusqlite::{Connection, OpenFlags};
use stable_value_format::{FILE_HEADER_BYTES, VALUE_ROW_BYTES, ValueId};
use stacks_common::util::hash::hex_bytes;

use crate::value_lookup::{Reference, ValueLookup};
use crate::{Result, source::Source};

/// Clone stopped, unpublished extraction files; never modify or resume the original attempt.
pub fn copy(seed: &Path, directory: &Path, name: &str) -> Result<()> {
    if !seed.join("INCOMPLETE").is_file() || seed.is_symlink() {
        return Err("reuse requires an explicitly retained private extraction".into());
    }
    let path = seed.join(name);
    Source::check_journals(&path)?;
    let db = open(&path)?;
    if db.query_row("SELECT version FROM marf_record_format", [], |r| {
        r.get::<_, i64>(0)
    })? != -6
    {
        return Err("reuse input is not an unpublished canonical extraction".into());
    }
    let generation: String = db.query_row(
        "SELECT path FROM clarity_stable_format WHERE singleton=1 AND version=1",
        [],
        |r| r.get(0),
    )?;
    let component = Path::new(&generation);
    if component.components().count() != 1
        || !matches!(component.components().next(), Some(Component::Normal(_)))
    {
        return Err("reuse generation must be a sibling".into());
    }
    // This detects interrupted SQL writes before they are used as extraction evidence.
    let checked: String = db.query_row("PRAGMA quick_check", [], |r| r.get(0))?;
    if checked != "ok" {
        return Err(format!("reuse SQL integrity: {checked}").into());
    }
    for entry in [name.to_owned(), generation, format!("{name}.blobs")] {
        copy_tree(&seed.join(&entry), &directory.join(entry))?;
    }
    Ok(())
}

/// Copy immutable seed files with metadata checks; reject links and unsupported entries.
fn copy_tree(source: &Path, destination: &Path) -> Result<()> {
    let before = fs::symlink_metadata(source)?;
    if before.is_dir() {
        fs::create_dir(destination)?;
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            copy_tree(&entry.path(), &destination.join(entry.file_name()))?;
        }
    } else if before.is_file() {
        if destination.exists() {
            return Err("reuse destination already exists".into());
        }
        reflink_copy::reflink_or_copy(source, destination)?;
        let after = fs::symlink_metadata(source)?;
        if before.len() != after.len() || before.modified()? != after.modified()? {
            return Err("reuse input changed while copying".into());
        }
    } else {
        return Err("reuse input contains a symlink or special file".into());
    }
    Ok(())
}

/// Open private SQL read-only so validating a seed cannot perform recovery writes.
fn open(path: &Path) -> Result<Connection> {
    Ok(Connection::open_with_flags(
        sqlite_readonly_uri(path, true)?,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )?)
}

/// Verify complete source-key coverage and every retained external value before reuse.
pub fn verify(source: &Connection, path: &Path, lookup: &ValueLookup) -> Result<()> {
    let format = binary_value_store::detect(source)?;
    let mut statement = source.prepare("SELECT key FROM data_table ORDER BY key")?;
    let mut input = statement.query([])?;
    let mut count = 0usize;
    let mut external = 0u64;
    while let Some(row) = input.next()? {
        let bytes = match format {
            ValueStorageFormat::LegacyText => hex_bytes(row.get_ref(0)?.as_str()?)?,
            ValueStorageFormat::BinaryV1 => row.get_ref(0)?.as_blob()?.to_vec(),
        };
        let key = MARFValue(bytes.try_into().map_err(|_| "invalid source commitment")?);
        match lookup.get(&key)? {
            Reference::Raw => {
                return Err("retained extraction is missing a source commitment".into());
            }
            Reference::Stable(_) => external += 1,
            Reference::Inline(_) => {}
        }
        count += 1;
        if count % 1_000_000 == 0 {
            eprintln!("canonical verifying_reused_keys={count}");
        }
    }
    if count != lookup.len() {
        return Err("retained extraction contains extra commitments".into());
    }
    let db = open(path)?;
    let generation: String =
        db.query_row("SELECT path FROM clarity_stable_format", [], |r| r.get(0))?;
    let length = File::open(
        path.parent()
            .ok_or("missing parent")?
            .join(generation)
            .join("value-directory.dat"),
    )?
    .metadata()?
    .len();
    if length != FILE_HEADER_BYTES as u64 + external * VALUE_ROW_BYTES as u64 {
        return Err("retained extraction has incomplete or unindexed directory rows".into());
    }
    drop(db);
    let workers = thread::available_parallelism()
        .map_or(1, |n| n.get())
        .min(8);
    let chunk = external.div_ceil(workers as u64).max(1);
    let progress = AtomicU64::new(0);
    thread::scope(|scope| {
        let mut handles = Vec::new();
        for worker in 0..workers {
            let start = (1 + worker as u64 * chunk).min(external + 1);
            let end = (start + chunk).min(external + 1);
            let progress = &progress;
            handles.push(scope.spawn(move || {
                verify_values(path, lookup, start, end, progress).map_err(|error| error.to_string())
            }));
        }
        for handle in handles {
            handle
                .join()
                .map_err(|_| "retained value audit worker panicked")??;
        }
        Ok::<(), String>(())
    })?;
    if progress.load(Ordering::Relaxed) != external {
        return Err("incomplete retained value audit".into());
    }
    Ok(())
}

/// Verify an ID interval through immutable runtime mappings with bounded descriptor caches.
fn verify_values(
    path: &Path,
    lookup: &ValueLookup,
    start: u64,
    end: u64,
    progress: &AtomicU64,
) -> Result<()> {
    let db = open(path)?;
    let ValueBackend::Stable(mut store) =
        ValueBackend::open_registered(&db, path)?.ok_or("missing retained values")?;
    for raw in start..end {
        let id = u32::try_from(raw)?;
        let value = store.read(ValueId::new(id)?)?;
        let key = value.commitment.clone();
        let text = StableMappedValueRecord::from_stable_parts(value).canonical()?;
        if MARFValue::from_value(&text) != key
            || !matches!(lookup.get(&key)?, Reference::Stable(found) if found == id)
        {
            return Err(
                format!("retained value reconstruction or membership mismatch at {id}").into(),
            );
        }
        let complete = progress.fetch_add(1, Ordering::Relaxed) + 1;
        if complete % 1_000_000 == 0 {
            eprintln!("canonical verifying_reused_values={complete}");
        }
    }
    Ok(())
}
