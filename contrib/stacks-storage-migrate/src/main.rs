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

//! Offline canonical MARF migration command.

use std::env;
use std::path::PathBuf;
use std::process::ExitCode;

use stacks_storage_migrate::{
    Config, Result, migrate, migrate_chainstate, migrate_reusing_clarity,
};

/// Parse explicit paths and policy, then publish a verified new directory.
fn run() -> Result<()> {
    let mut args = env::args().skip(1);
    let chainstate = match args.next().as_deref() {
        Some("--help" | "-h") | None => {
            println!(
                "stacks-storage-migrate marf --source <marf.sqlite> --destination <new-directory> [--value-store clarity] [--max-trie-bytes <bytes>] [--reuse-clarity-extraction <stopped-private-directory>]\n\nstacks-storage-migrate chainstate --source <offline-root> --destination <new-root> [--max-trie-bytes <bytes>]\n\nThe source must be offline and checkpointed. The destination must not exist."
            );
            return Ok(());
        }
        Some("marf") => false,
        Some("chainstate") => true,
        _ => return Err("expected 'marf' or 'chainstate'; use --help for usage".into()),
    };
    let mut source = None;
    let mut destination = None;
    let mut clarity_values = false;
    let mut retained = None;
    let mut max_trie_bytes = 512 * 1024 * 1024;
    while let Some(flag) = args.next() {
        let value = args
            .next()
            .ok_or_else(|| format!("missing value for {flag}"))?;
        match flag.as_str() {
            "--reuse-clarity-extraction" if retained.is_none() => {
                retained = Some(PathBuf::from(value))
            }
            "--source" if source.is_none() => source = Some(PathBuf::from(value)),
            "--destination" if destination.is_none() => destination = Some(PathBuf::from(value)),
            "--value-store" if value == "clarity" && !clarity_values => clarity_values = true,
            "--max-trie-bytes" => max_trie_bytes = value.parse()?,
            _ => return Err(format!("unsupported or duplicate argument {flag}").into()),
        }
    }
    let source = source.ok_or("--source is required")?;
    let destination = destination.ok_or("--destination is required")?;
    if chainstate {
        if clarity_values || retained.is_some() {
            return Err("chainstate mode selects value ownership from its schema registry".into());
        }
        let count = migrate_chainstate(&source, &destination, max_trie_bytes)?;
        println!("published={} marfs={count}", destination.display());
        return Ok(());
    }
    let config = Config {
        source,
        destination,
        clarity_values,
        max_trie_bytes,
    };
    let result = match retained {
        Some(path) => migrate_reusing_clarity(&config, &path)?,
        None => migrate(&config)?,
    };
    println!(
        "published={} tries={} input_trie_bytes={} output_trie_bytes={}",
        result.database.display(),
        result.tries,
        result.source_bytes,
        result.destination_bytes
    );
    Ok(())
}

/// Preserve concrete failure messages and return a nonzero status without publishing partial work.
fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("stacks-storage-migrate: {error}");
            ExitCode::FAILURE
        }
    }
}
