# Deployment and maintenance

## Run a persistent node

Build the release binary and run it under your process supervisor with a stable working
directory, explicit configuration path and operator-owned data directory. Use the minimum
filesystem permissions needed to read nominated sources and write catalog/cache state.
The binary embeds the web UI; no separate frontend service is required.

A minimal LAN configuration, adapted to your host, is:

```toml
[server]
bind = "0.0.0.0:7099"
public_url = "http://workstation.lan:7099"
data_dir = "/var/lib/hoard"

[node]
name = "workstation"
discovery = "none"
```

Set tokens in the service environment or a protected `.env`. Do not commit them. Configure
TLS and network access at the deployment boundary when required. Hoard has no per-user
read authorization: anyone who can reach its resource endpoints can browse and download.
The node token protects mutations only. Index only folders appropriate for that audience.

Open `/healthz` and the configured public URL, then test discovery/download or cloning from
a receiving machine. An address marked configured is not a reachability test. Review
[configuration precedence](../user-guide/configuration.md) when managed settings exist.

## Upgrades and backups

Hoard is pre-release. Its catalog compatibility policy permits rebuilding `native/` when
the persisted format differs from the binary. That can discard shelves, loadouts, verdicts,
usage history and other catalog-only state even though source files remain intact. A search
index rebuild from the existing catalog is different from a catalog rebuild from sources.

Before upgrading, stop the owning process cleanly and back up its configuration, environment
secrets, complete data directory and any custom index/cache locations. Retain the matching
binary and source version. Back up nominated source files separately. Restore into an
isolated location first and verify resources and catalog-only state before changing service
paths. Do not promise catalog migration or rebuild preservation that the implementation does
not provide.

## Diagnostics and maintenance

### Search-schema upgrades

Restart Hoard after installing a rebuilt release binary so the running backend and embedded
UI match the checkout. At startup, `Catalog::open_or_scan` compares the derived search schema
with the current one. An older search index is rebuilt from existing catalog metadata,
without rereading model weights or discarding shelves, loadouts, verdicts, hashes or IDs.
This is separate from the persisted catalog-format rebuild described above.

A rescan discovers filesystem changes; a search rebuild indexes metadata already in the
catalog. Diagnose which layer is missing before choosing either operation. A file that was
scanned but cannot be recognized by its folder or alias needs path/search investigation,
not a move or rename of the original weights. Keep the same configuration and data directory
when restarting so Hoard opens the existing catalog.

### Routine diagnostics

- `/healthz` reports server, upstream and watcher health; use service logs for detailed errors.
- `hoard doctor` inspects the configured catalog and reports lock, format and integrity issues.
- Catalog commands require exclusive ownership. Stop the service before maintenance instead
  of opening the same catalog from a second process.
- Search rebuilds use canonical catalog records. Compaction is an explicit maintenance operation;
  inspect `hoard catalog --help` and the [catalog reference](../reference/native-catalog.md).
- Removing a source leaves its disk files intact and persists the exclusion. Clearing finished
  scans changes history only. Re-add a removed source through Sources when appropriate.

Keep test nodes on temporary data directories and available loopback ports. Never reuse a
production catalog for a smoke test or stop unrelated processes to free a port. Validate
upgrades and backups on the deployment's actual workload; historical benchmark results are
not a service-level guarantee.
