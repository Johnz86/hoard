use std::path::Path;
use std::sync::Arc;

use hoard_index::catalog::Catalog;
use hoard_index::config::IndexConfig;

/// A native catalog with no configured sources, answering under `node`, for the test worlds
/// that only need the server state to be holding one.
pub(crate) fn empty_catalog(dir: &Path, node: &str) -> Arc<Catalog> {
    let config = IndexConfig {
        data_dir: dir.join("native-data"),
        ..IndexConfig::default()
    };
    Arc::new(Catalog::open_or_scan(&config, node, |_, _| {}).expect("native catalog opens"))
}
