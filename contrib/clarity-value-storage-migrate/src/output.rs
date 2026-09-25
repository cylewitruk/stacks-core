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

//! Console presentation for structured migration-engine events.

use stackslib::clarity_vm::database::binary_value_store::{
    DataMigrationStats, MigrationConfig, MigrationEvent, MigrationPhase, ShapeCacheStats,
};

/// Print the source and destination selected for one migration invocation.
pub fn print_configuration(config: &MigrationConfig) {
    println!("source={}", config.source.display());
    println!("destination={}", config.destination.display());
}

/// Render one structured engine event in the tool's machine-readable format.
pub fn print_event(event: MigrationEvent) {
    match event {
        MigrationEvent::PhaseStarted(phase) => println!("phase={}", phase_name(phase)),
        MigrationEvent::TableCopied {
            table,
            rows,
            elapsed,
        } => println!(
            "table={table} rows={rows} seconds={:.3}",
            elapsed.as_secs_f64()
        ),
        MigrationEvent::MetadataCopied { rows } => println!("metadata_rows={rows}"),
        MigrationEvent::ValuesProgress { rows } => println!("migrated_rows={rows}"),
        MigrationEvent::ValuesCopied {
            stats,
            cache,
            elapsed,
        } => {
            println!("data_seconds={:.3}", elapsed.as_secs_f64());
            println!(
                "data_rows_per_second={:.1}",
                stats.rows as f64 / elapsed.as_secs_f64()
            );
            print_data_stats(&stats, cache);
        }
        MigrationEvent::IndexesCreated { elapsed } => {
            println!("index_seconds={:.3}", elapsed.as_secs_f64());
        }
        MigrationEvent::VacuumCompleted { elapsed } => {
            println!("vacuum_seconds={:.3}", elapsed.as_secs_f64());
        }
        MigrationEvent::SemanticIntegrityProgress { rows } => {
            println!("semantic_integrity_rows={rows}");
        }
        MigrationEvent::SemanticIntegrityCompleted { rows, elapsed } => println!(
            "semantic_integrity_rows={rows} semantic_integrity_seconds={:.3}",
            elapsed.as_secs_f64()
        ),
        MigrationEvent::DestinationCompleted { sqlite_bytes } => {
            println!("complete destination_sqlite_bytes={sqlite_bytes}");
        }
        MigrationEvent::CutoverCompleted { active, backup } => println!(
            "cutover_complete active={} backup={}",
            active.display(),
            backup.display()
        ),
    }
}

/// Map a structured phase to the stable CLI label used by scripts.
fn phase_name(phase: MigrationPhase) -> &'static str {
    match phase {
        MigrationPhase::CreateSchema => "create_schema",
        MigrationPhase::CopyMarfDomain => "copy_marf_domain",
        MigrationPhase::CopyClarityMetadata => "copy_clarity_metadata",
        MigrationPhase::CopyClarityValues => "copy_clarity_values",
        MigrationPhase::CreateIndexes => "create_indexes",
        MigrationPhase::Verify => "verify",
        MigrationPhase::Vacuum => "vacuum",
        MigrationPhase::Analyze => "analyze",
        MigrationPhase::Integrity => "integrity",
        MigrationPhase::SemanticIntegrity => "semantic_integrity",
        MigrationPhase::Reopen => "reopen",
        MigrationPhase::Cutover => "cutover",
    }
}

/// Print final value and shape-cache counters and density ratios.
fn print_data_stats(stats: &DataMigrationStats, cache: ShapeCacheStats) {
    println!("data_rows={}", stats.rows);
    println!("packed_rows={}", stats.packed_rows);
    println!("reversible_rows={}", stats.reversible_rows);
    println!("source_key_bytes={}", stats.source_key_bytes);
    println!("source_value_bytes={}", stats.source_value_bytes);
    println!("destination_key_bytes={}", stats.destination_key_bytes);
    println!(
        "destination_record_bytes={}",
        stats.destination_record_bytes
    );
    println!("shape_cache_hits={}", cache.hits);
    println!("shape_cache_misses={}", cache.misses);
    println!("shape_cache_evictions={}", cache.evictions);
    if stats.source_value_bytes != 0 {
        println!(
            "record/source_value_ratio={:.6}",
            stats.destination_record_bytes as f64 / stats.source_value_bytes as f64
        );
    }
    let source_bytes = stats.source_key_bytes + stats.source_value_bytes;
    let destination_bytes = stats.destination_key_bytes + stats.destination_record_bytes;
    if source_bytes != 0 {
        println!(
            "destination/source_key_value_ratio={:.6}",
            destination_bytes as f64 / source_bytes as f64
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phase_labels_match_the_script_facing_contract() {
        let cases = [
            (MigrationPhase::CreateSchema, "create_schema"),
            (MigrationPhase::CopyMarfDomain, "copy_marf_domain"),
            (MigrationPhase::CopyClarityMetadata, "copy_clarity_metadata"),
            (MigrationPhase::CopyClarityValues, "copy_clarity_values"),
            (MigrationPhase::CreateIndexes, "create_indexes"),
            (MigrationPhase::Verify, "verify"),
            (MigrationPhase::Vacuum, "vacuum"),
            (MigrationPhase::Analyze, "analyze"),
            (MigrationPhase::Integrity, "integrity"),
            (MigrationPhase::SemanticIntegrity, "semantic_integrity"),
            (MigrationPhase::Reopen, "reopen"),
            (MigrationPhase::Cutover, "cutover"),
        ];
        for (phase, expected) in cases {
            assert_eq!(phase_name(phase), expected);
        }
    }
}
