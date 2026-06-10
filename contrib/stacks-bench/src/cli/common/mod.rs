// Macros must be defined before submodules that use them, so `context`
// (which contains the macro definitions) comes first.
#[macro_use]
mod context;
mod cleanup;
mod format;
mod progress;
mod table;

pub use cleanup::run_db_cleanup;
#[allow(unused_imports)] // FAILURE_ICON referenced by fmt_failure! macro
pub use context::{
    BoxedOutput, CliContext, CommandResult, ExecCommand, FAILURE_ICON, SUCCESS_ICON, boxed,
    serialize_erased,
};
pub use format::{
    fmt_duration, fmt_relative_time, fmt_run_label, fmt_run_name_suffix, fmt_u64_thousands,
    parse_since,
};
pub use progress::run_indexer_progress_ui;
pub use table::{Align, Table};
