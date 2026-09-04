use std::collections::HashMap;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config::CleanStrategy;
use crate::error::Result;

/// What eviction knows about one cache unit: a block-cache entry directory, or a cached API
/// response and its `.body` sidecar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryStat {
    pub path: PathBuf,
    pub size: u64,
    /// Seconds since the epoch of the last read or write, from the entry's access marker.
    pub accessed: i64,
    /// Seconds since the epoch of when the unit first appeared.
    pub created: i64,
}

/// What one eviction pass did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EvictionReport {
    pub units: usize,
    pub total_bytes: u64,
    pub freed_bytes: u64,
    pub removed: usize,
}

/// Chooses whole units to remove so the tree fits under `limit`.
///
/// Nothing is selected while the total is already within the limit. Units are removed in the
/// strategy's order until the remaining total fits, so a single unit larger than the limit is
/// removed along with everything else rather than left behind.
pub fn select_evictions(
    entries: &[EntryStat],
    limit: u64,
    strategy: CleanStrategy,
) -> Vec<PathBuf> {
    let mut total: u64 = entries.iter().map(|entry| entry.size).sum();
    if total <= limit {
        return Vec::new();
    }

    let mut ordered: Vec<&EntryStat> = entries.iter().collect();
    match strategy {
        CleanStrategy::Lru => ordered.sort_by(|a, b| {
            a.accessed
                .cmp(&b.accessed)
                .then_with(|| a.path.cmp(&b.path))
        }),
        CleanStrategy::Fifo => {
            ordered.sort_by(|a, b| a.created.cmp(&b.created).then_with(|| a.path.cmp(&b.path)))
        }
        CleanStrategy::LargeFirst => {
            ordered.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.path.cmp(&b.path)))
        }
    }

    let mut victims = Vec::new();
    for entry in ordered {
        if total <= limit {
            break;
        }
        total -= entry.size;
        victims.push(entry.path.clone());
    }
    victims
}

/// Scans `root`, evicts whole units until the tree fits under `limit` and reports what it
/// did. The hourly timer that calls this belongs to the server.
pub async fn run_once(root: &Path, limit: u64, strategy: CleanStrategy) -> Result<EvictionReport> {
    let root = root.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let entries = collect(&root)?;
        let total_bytes = entries.iter().map(|entry| entry.size).sum();
        let victims = select_evictions(&entries, limit, strategy);
        let mut report = EvictionReport {
            units: entries.len(),
            total_bytes,
            freed_bytes: 0,
            removed: 0,
        };
        let sizes: HashMap<&Path, u64> = entries
            .iter()
            .map(|entry| (entry.path.as_path(), entry.size))
            .collect();
        for victim in &victims {
            let size = sizes.get(victim.as_path()).copied().unwrap_or_default();
            if remove_unit(victim)? {
                report.freed_bytes += size;
                report.removed += 1;
            }
        }
        Ok(report)
    })
    .await
    .map_err(|error| std::io::Error::other(error.to_string()))?
}

/// Walks the cache tree once, attributing every file either to the block-cache entry that
/// owns it or to a loose cached API response.
pub fn collect(root: &Path) -> Result<Vec<EntryStat>> {
    let mut units = Vec::new();
    collect_into(root, &mut units)?;
    Ok(units)
}

fn collect_into(dir: &Path, units: &mut Vec<EntryStat>) -> Result<()> {
    let mut children = Vec::new();
    let read = match std::fs::read_dir(dir) {
        Ok(read) => read,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let mut has_meta = false;
    let mut files = Vec::new();
    for entry in read {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            children.push(path);
            continue;
        }
        if path.file_name().is_some_and(|name| name == "meta.bin") {
            has_meta = true;
        }
        files.push(path);
    }

    if has_meta {
        units.push(entry_stat(dir)?);
        return Ok(());
    }
    for file in files {
        if file
            .extension()
            .is_some_and(|extension| extension == "json")
        {
            units.push(response_stat(&file)?);
        }
    }
    for child in children {
        collect_into(&child, units)?;
    }
    Ok(())
}

fn entry_stat(dir: &Path) -> Result<EntryStat> {
    let size = directory_size(dir)?;
    let marker = dir.join("meta.lock");
    let accessed = seconds_modified(&marker).or_else(|| seconds_modified(&dir.join("meta.bin")));
    let created = seconds_created(&dir.join("meta.bin"));
    Ok(EntryStat {
        path: dir.to_path_buf(),
        size,
        accessed: accessed.unwrap_or_default(),
        created: created.or(accessed).unwrap_or_default(),
    })
}

fn response_stat(json: &Path) -> Result<EntryStat> {
    let body = crate::cache::layout::body_path(json);
    let size = file_size(json) + file_size(&body);
    let accessed = seconds_modified(json).unwrap_or_default();
    Ok(EntryStat {
        path: json.to_path_buf(),
        size,
        accessed,
        created: seconds_created(json).unwrap_or(accessed),
    })
}

fn remove_unit(path: &Path) -> Result<bool> {
    let removed = if path.is_dir() {
        std::fs::remove_dir_all(path).is_ok()
    } else {
        let body = crate::cache::layout::body_path(path);
        let _ = std::fs::remove_file(&body);
        std::fs::remove_file(path).is_ok()
    };
    Ok(removed)
}

fn directory_size(dir: &Path) -> Result<u64> {
    let mut total = 0;
    let read = match std::fs::read_dir(dir) {
        Ok(read) => read,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error.into()),
    };
    for entry in read {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            total += directory_size(&entry.path())?;
        } else {
            total += entry.metadata().map_or(0, |meta| meta.len());
        }
    }
    Ok(total)
}

fn file_size(path: &Path) -> u64 {
    std::fs::metadata(path).map_or(0, |meta| meta.len())
}

fn seconds_modified(path: &Path) -> Option<i64> {
    seconds(std::fs::metadata(path).ok()?.modified().ok()?)
}

fn seconds_created(path: &Path) -> Option<i64> {
    seconds(std::fs::metadata(path).ok()?.created().ok()?)
}

fn seconds(time: SystemTime) -> Option<i64> {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|since| i64::try_from(since.as_secs()).ok())
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    fn stat(path: &str, size: u64, accessed: i64, created: i64) -> EntryStat {
        EntryStat {
            path: PathBuf::from(path),
            size,
            accessed,
            created,
        }
    }

    fn sample() -> Vec<EntryStat> {
        vec![
            stat("/repos/a", 100, 30, 1),
            stat("/repos/b", 500, 10, 2),
            stat("/repos/c", 400, 20, 3),
        ]
    }

    #[test]
    fn nothing_is_evicted_while_the_tree_fits() {
        assert!(select_evictions(&sample(), 1000, CleanStrategy::Lru).is_empty());
        assert!(select_evictions(&sample(), 1000, CleanStrategy::Fifo).is_empty());
        assert!(select_evictions(&sample(), 1000, CleanStrategy::LargeFirst).is_empty());
        assert!(select_evictions(&[], 0, CleanStrategy::Lru).is_empty());
    }

    #[test]
    fn lru_removes_the_least_recently_used_first() {
        let victims = select_evictions(&sample(), 600, CleanStrategy::Lru);
        assert_eq!(victims, vec![PathBuf::from("/repos/b")]);
    }

    #[test]
    fn fifo_removes_the_oldest_first() {
        let victims = select_evictions(&sample(), 600, CleanStrategy::Fifo);
        assert_eq!(
            victims,
            vec![PathBuf::from("/repos/a"), PathBuf::from("/repos/b")]
        );
    }

    #[test]
    fn large_first_removes_the_biggest_first() {
        let victims = select_evictions(&sample(), 600, CleanStrategy::LargeFirst);
        assert_eq!(victims, vec![PathBuf::from("/repos/b")]);
    }

    #[test]
    fn a_single_entry_over_the_limit_is_evicted() {
        let entries = vec![stat("/repos/huge", 5_000, 1, 1)];
        assert_eq!(
            select_evictions(&entries, 1_000, CleanStrategy::Lru),
            vec![PathBuf::from("/repos/huge")]
        );
        assert_eq!(
            select_evictions(&entries, 1_000, CleanStrategy::LargeFirst),
            vec![PathBuf::from("/repos/huge")]
        );
    }

    #[test]
    fn ties_break_on_the_path_so_the_order_is_stable() {
        let entries = vec![stat("/repos/z", 10, 5, 5), stat("/repos/a", 10, 5, 5)];
        assert_eq!(
            select_evictions(&entries, 10, CleanStrategy::Lru),
            vec![PathBuf::from("/repos/a")]
        );
    }

    #[tokio::test]
    async fn a_pass_over_a_real_tree_removes_whole_units() {
        let root = TempDir::new().expect("temp dir");
        let entry = root
            .path()
            .join("files/models/org/name/resolve/sha/model.bin");
        std::fs::create_dir_all(entry.join("blocks")).expect("entry dirs");
        std::fs::write(entry.join("meta.bin"), vec![0u8; 64]).expect("meta");
        std::fs::write(entry.join("meta.lock"), b"").expect("marker");
        std::fs::write(entry.join("blocks/block_00000000.bin"), vec![0u8; 4096]).expect("block");

        let api = root.path().join("api/models/org/name/revision/sha");
        std::fs::create_dir_all(&api).expect("api dirs");
        std::fs::write(api.join("meta_get.json"), b"{}").expect("json");
        std::fs::write(api.join("meta_get.json.body"), vec![0u8; 128]).expect("body");

        let units = collect(root.path()).expect("scan");
        assert_eq!(units.len(), 2);
        let total: u64 = units.iter().map(|unit| unit.size).sum();
        assert_eq!(total, 64 + 4096 + 2 + 128);

        let report = run_once(root.path(), 0, CleanStrategy::LargeFirst)
            .await
            .expect("eviction runs");
        assert_eq!(report.units, 2);
        assert_eq!(report.removed, 2);
        assert_eq!(report.total_bytes, total);
        assert_eq!(report.freed_bytes, total);
        assert!(!entry.exists());
        assert!(!api.join("meta_get.json").exists());
        assert!(!api.join("meta_get.json.body").exists());
    }

    #[tokio::test]
    async fn a_pass_under_the_limit_touches_nothing() {
        let root = TempDir::new().expect("temp dir");
        let entry = root
            .path()
            .join("files/models/org/name/resolve/sha/model.bin");
        std::fs::create_dir_all(entry.join("blocks")).expect("entry dirs");
        std::fs::write(entry.join("meta.bin"), vec![0u8; 64]).expect("meta");

        let report = run_once(root.path(), u64::MAX, CleanStrategy::Lru)
            .await
            .expect("eviction runs");
        assert_eq!(report.removed, 0);
        assert_eq!(report.freed_bytes, 0);
        assert!(entry.join("meta.bin").exists());
    }
}
