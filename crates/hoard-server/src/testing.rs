use std::path::Path;
use std::sync::Arc;

use hoard_index::catalog::Catalog;
use hoard_index::config::IndexConfig;

/// A native catalog with no configured sources, for the test worlds that only need the
/// server state to be holding one.
pub(crate) fn empty_catalog(dir: &Path) -> Arc<Catalog> {
    let config = IndexConfig {
        data_dir: dir.join("native-data"),
        ..IndexConfig::default()
    };
    Arc::new(Catalog::open_or_scan(&config, "test", |_, _| {}).expect("native catalog opens"))
}
