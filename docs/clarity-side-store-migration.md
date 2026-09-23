# Clarity Binary Side-Store Migration

`clarity-value-storage-migrate` rebuilds a legacy Clarity `marf.sqlite` into
the Binary V1 schema. The operation is offline: stop the node before starting
it. Normal node startup detects either completed format automatically.

The dedicated tool crate under `contrib/clarity-value-storage-migrate` owns
command-line parsing and console presentation. It drives a silent
stackslib-owned migration engine through structured configuration, progress
events, and a final report. Binary V1 schema creation and validation share one
authoritative definition in
`clarity_vm::database::binary_value_store`; migration and snapshot code do not
carry independent copies of the production schema.

## Requirements

- Stop every process that can write the source database.
- Checkpoint any WAL and ensure `<source>-wal` and `<source>-journal` are absent
  or empty.
- Give the migrator read/write access to the source. It does not change source
  rows; write access is used to reserve SQLite's writer slot for the full copy.
- Keep the source's `<source>.blobs` external-trie file beside the SQLite file.
  The destination must be on the same filesystem so the migrator can give it a
  separate hard-link name without copying the trie data.
- Reserve space for the new SQLite database while retaining the legacy source.
  The measured mainnet SQLite files were 38.56 GiB for the destination and
  147.72 GiB for the source. These figures exclude the shared `.blobs` file.

## Build and Verify a Sibling Database

Build the dedicated tool without adding CLI dependencies to `stackslib`:

```text
cargo build --release -p clarity-value-storage-migrate
```

```text
clarity-value-storage-migrate \
  --source /path/to/clarity/marf.sqlite \
  --destination /path/to/clarity/marf.binary-v1.sqlite \
  --cache-mib 1024
```

The tool streams into a newly created database, builds deferred indexes,
runs `quick_check`, restores durable SQLite settings, marks the format
complete, and reopens the SQLite and `.blobs` pair through the production MARF
reader. It reads the latest confirmed root before reporting success. The tool
refuses to overwrite either destination file or copy an unclassified SQLite
table.

Use `--full-integrity` for release qualification or forensic verification. It
runs SQLite `integrity_check`, reconstructs every stored value, audits the
packed representation, and re-derives every content-addressed key. A final
`VACUUM` is normally unnecessary for the append-only rebuild; `--vacuum`
exists for explicit diagnostics.

If row streaming fails, remove the incomplete destination SQLite file and its
`.blobs` hard link, then restart. If all rows and indexes were written but
publication was interrupted, retain both destination files and rerun with
`--resume-finalize` to repeat verification and publication without
retranscoding the rows.

## Cut Over

After verification, either stop the tool and manage filenames manually or
request a same-directory cutover:

```text
clarity-value-storage-migrate \
  --source /path/to/clarity/marf.sqlite \
  --destination /path/to/clarity/marf.binary-v1.sqlite \
  --resume-finalize \
  --full-integrity \
  --cutover-backup /path/to/clarity/marf.legacy.sqlite
```

The tool fsyncs the completed destination pair, preserves the legacy SQLite
database and a `.blobs` link at the explicit backup path, activates Binary V1,
fsyncs the directory, and reopens the active MARF. Activation failures restore
the source and destination names; if restoration also fails, the error reports
the source, destination, backup, and each failed step for manual recovery.

Start the node only after cutover succeeds. Retain the legacy backup until the
new node has completed the operator's normal validation window, then remove it
manually. Binary V1 is a roll-forward migration; downgrade is not supported.
