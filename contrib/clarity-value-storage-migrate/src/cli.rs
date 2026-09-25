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

//! Command-line definition and conversion to the migration engine's API.

use std::path::PathBuf;

use clap::Parser;
use stackslib::clarity_vm::database::binary_value_store::{
    DEFAULT_CACHE_MIB, IntegrityLevel, MAX_CACHE_MIB, MigrationConfig, MigrationMode,
};

/// Offline Clarity side-store migration arguments.
#[derive(Debug, Parser)]
#[command(
    name = "clarity-value-storage-migrate",
    about = "Migrate a legacy Clarity side store to the Binary V1 format"
)]
pub struct Cli {
    /// Legacy Clarity `marf.sqlite` database.
    #[arg(long, value_name = "PATH")]
    source: PathBuf,
    /// New Binary V1 database on the source filesystem to create or finalize.
    #[arg(long, value_name = "PATH")]
    destination: PathBuf,
    /// Verify and publish an existing incomplete destination.
    #[arg(long)]
    resume_finalize: bool,
    /// Compact the completed destination before publication.
    #[arg(long)]
    vacuum: bool,
    /// Run full SQLite and record-level semantic integrity checks.
    #[arg(long)]
    full_integrity: bool,
    /// SQLite page-cache and mmap budget in mebibytes.
    #[arg(
        long,
        value_name = "MIB",
        default_value_t = DEFAULT_CACHE_MIB,
        value_parser = clap::value_parser!(u64).range(16..=MAX_CACHE_MIB)
    )]
    cache_mib: u64,
    /// Preserve the legacy source here and atomically activate the destination.
    #[arg(long, value_name = "PATH")]
    cutover_backup: Option<PathBuf>,
}

impl Cli {
    /// Convert presentation-layer flags into the engine's domain configuration.
    pub fn into_config(self) -> MigrationConfig {
        MigrationConfig {
            source: self.source,
            destination: self.destination,
            vacuum: self.vacuum,
            integrity: if self.full_integrity {
                IntegrityLevel::Full
            } else {
                IntegrityLevel::Quick
            },
            cache_mib: self.cache_mib,
            mode: if self.resume_finalize {
                MigrationMode::ResumeFinalize
            } else {
                MigrationMode::Create
            },
            cutover_backup: self.cutover_backup,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_complete_migration_configuration() {
        let cli = Cli::try_parse_from([
            "clarity-value-storage-migrate",
            "--source",
            "source.sqlite",
            "--destination",
            "destination.sqlite",
            "--resume-finalize",
            "--vacuum",
            "--full-integrity",
            "--cache-mib",
            "2048",
            "--cutover-backup",
            "legacy.sqlite",
        ])
        .unwrap();

        assert_eq!(
            cli.into_config(),
            MigrationConfig {
                source: "source.sqlite".into(),
                destination: "destination.sqlite".into(),
                vacuum: true,
                integrity: IntegrityLevel::Full,
                cache_mib: 2_048,
                mode: MigrationMode::ResumeFinalize,
                cutover_backup: Some("legacy.sqlite".into()),
            }
        );
    }

    #[test]
    fn defaults_to_create_with_quick_integrity() {
        let config = Cli::try_parse_from([
            "clarity-value-storage-migrate",
            "--source",
            "source.sqlite",
            "--destination",
            "destination.sqlite",
        ])
        .unwrap()
        .into_config();

        assert_eq!(config.mode, MigrationMode::Create);
        assert_eq!(config.integrity, IntegrityLevel::Quick);
        assert_eq!(config.cache_mib, DEFAULT_CACHE_MIB);
        assert!(!config.vacuum);
        assert_eq!(config.cutover_backup, None);
    }

    #[test]
    fn rejects_missing_paths_unknown_arguments_and_invalid_cache() {
        assert!(Cli::try_parse_from(["clarity-value-storage-migrate"]).is_err());
        assert!(Cli::try_parse_from(["clarity-value-storage-migrate", "--unknown"]).is_err());
        assert!(
            Cli::try_parse_from([
                "clarity-value-storage-migrate",
                "--source",
                "source",
                "--destination",
                "destination",
                "--cache-mib",
                "1",
            ])
            .is_err()
        );
    }
}
