//! Offline immutable-base construction on a disposable database clone.
use std::path::Path;
fn main() -> extent_ptrhash::Result<()> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 3 { return Err("usage: build-extent-ptrhash CLONE_DB NEW_INDEX_DIRECTORY".into()); }
    extent_ptrhash::build_and_activate(Path::new(&args[1]), Path::new(&args[2]))
}
