# Configuration

Hoard starts without a configuration file. CLI `--config PATH` (or `HOARD_CONFIG`) selects an
explicit TOML file; otherwise `./hoard.toml` is read when present. See
[hoard.example.toml](../../hoard.example.toml) for the supported sections and fields. Remove
its example source entries before adapting it to your own machine.

## Persistent settings

| Location | Purpose | How it is changed |
| --- | --- | --- |
| `hoard.toml` | Listener, data directory, sources, mirror, discovery and indexing policy | Edit the selected configuration and restart |
| `<data_dir>/sources.json` | Folders added through the web UI | Sources ADD FOLDER |
| `<data_dir>/sources.disabled.json` | Persistent exclusions for removed sources | Sources REMOVE; adding the name again clears its exclusion |
| `<data_dir>/node.json` | Managed node name and public URL overrides | Sources SHARING & SETUP; address applies immediately, node name after restart |
| Server environment or `.env` | Tokens and other sensitive values | Operator-managed; never commit credentials |

Managed node values override TOML name/public URL on startup. Deleting `node.json` returns
control to TOML/defaults. A managed source with the same name replaces its TOML definition;
persisted exclusions mask either origin. Removal does not rewrite TOML or delete source files.

The default data directory is `./data`, relative to the server working directory. Keep that
working directory stable or use an absolute `[server] data_dir`. The index and mirror normally
share that root; deployments with a custom index directory or mirror cache must back up and
operate those locations explicitly.

## Listener and advertised address

`[server] bind` controls listening; `public_url` controls links and client commands. CLI
`serve --bind` overrides the listener. The default listener is `0.0.0.0:7099`; the default
local command URL uses loopback. LAN discovery retains its hostname resolution for wildcard
listeners. A loopback listener accepts connections only from that machine.

Saving a sharing address updates generated links and commands immediately and notifies
discovery in the background. It does not rebind or restart the service or change network
settings. A saved node name remains pending until restart. Test a changed address from a
receiving machine; saving it does not establish reachability.

## Access and network behavior

`HOARD_NODE_TOKEN` protects catalog/configuration mutations. The browser's owner token stays
in tab memory and is cleared on reload. Browsing and downloads remain public to clients that
can reach the node; without a write token, mutations are open too. `HF_TOKEN` is an upstream
credential, not a user login. Do not treat `[node] serve` as an HTTP access-control switch.

`[mirror] offline = true` disables upstream access. Discovery can be `mdns`, `static` or
`none`; static peers are the fallback when multicast is unavailable. Sources are nominated
server paths, not browser uploads. Watching and periodic reconciliation keep their index
current. See [the API](../reference/api/hoard.md) for source and scan lifecycle semantics.
