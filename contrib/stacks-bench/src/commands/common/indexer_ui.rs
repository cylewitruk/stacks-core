use anyhow::Result;
use stacks_bench::indexer::IndexerEvent;
use tokio::sync::mpsc;

/// Spawner for indexer event consumers. The caller provides this to control
/// how indexer progress is rendered (CLI progress bar, MCP notifications,
/// or silent drain).
///
/// Implementations must be re-invocable so multi-target benchmark runs can
/// drive one UI session per indexed window.
pub type IndexerUiSpawner = Box<
    dyn Fn(
            mpsc::UnboundedReceiver<IndexerEvent>,
            u64, // start_height
            u64, // end_height
            u64, // tip_height
        ) -> tokio::task::JoinHandle<Result<()>>
        + Send
        + Sync,
>;

/// Returns an [`IndexerUiSpawner`] that silently drains all events.
pub fn silent_indexer_ui() -> IndexerUiSpawner {
    Box::new(|rx, _, _, _| {
        tokio::spawn(async move {
            let mut rx = rx;
            while rx.recv().await.is_some() {}
            Ok(())
        })
    })
}
