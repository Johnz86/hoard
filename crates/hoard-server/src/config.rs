use std::collections::BTreeSet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use url::{Host, Url};

use hoard_index::config::{IndexConfig, SourceConfig};
use hoard_mirror::config::MirrorConfig;
use hoard_mirror::lookup::LookupConfig;

use crate::error::ApiError;

fn default_bind() -> SocketAddr {
    SocketAddr::from(([0, 0, 0, 0], 7099))
}

fn default_data_dir() -> PathBuf {
    PathBuf::from("./data")
}

fn default_node_name() -> String {
    hostname().unwrap_or_else(|| "hoard".to_owned())
}

const fn default_true() -> bool {
    true
}

/// The `[server]` section.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_bind")]
    pub bind: SocketAddr,
    /// The URL other machines reach this node on; it is what the pull commands print.
    #[serde(default)]
    pub public_url: Option<String>,
    #[serde(default = "default_data_dir")]
    pub data_dir: PathBuf,
    /// Timeout for metadata routes. File streaming is deliberately never timed out here.
    #[serde(default = "default_metadata_timeout")]
    pub metadata_timeout_secs: u64,
    /// Directories outside `data_dir/profiles` that a projection may write symlinks into.
    ///
    /// A projection is a write to this machine's filesystem asked for over HTTP, so the
    /// places it may touch are named here rather than in the request. A tool's own model
    /// directory belongs in this list; nothing is outside `data_dir/profiles` without it.
    #[serde(default)]
    pub projection_roots: Vec<PathBuf>,
}

const fn default_metadata_timeout() -> u64 {
    30
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: default_bind(),
            public_url: None,
            data_dir: default_data_dir(),
            metadata_timeout_secs: default_metadata_timeout(),
            projection_roots: Vec::new(),
        }
    }
}

impl ServerConfig {
    /// Uses the configured URL, or a local address when listening on all interfaces.
    pub fn resolved_public_url(&self) -> String {
        if let Some(url) = &self.public_url {
            return url.clone();
        }
        let mut address = self.bind;
        if address.ip().is_unspecified() {
            address.set_ip(match address.ip() {
                IpAddr::V4(_) => IpAddr::V4(Ipv4Addr::LOCALHOST),
                IpAddr::V6(_) => IpAddr::V6(Ipv6Addr::LOCALHOST),
            });
        }
        format!("http://{address}")
    }

    pub(crate) fn discovery_url(&self) -> String {
        self.public_url
            .clone()
            .unwrap_or_else(|| format!("http://{}", self.bind))
    }
}

/// Settings saved by the web UI take effect together on the next process start.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeSettings {
    pub node_name: String,
    pub public_url: String,
}

impl NodeSettings {
    pub(crate) fn validated(self) -> Result<Self, ApiError> {
        let node_name = self.node_name.trim();
        if node_name.is_empty()
            || node_name.chars().count() > hoard_net::identity::MAX_NAME
            || node_name.chars().any(char::is_control)
        {
            return Err(ApiError::bad_request(format!(
                "node name must contain 1 to {} characters without line breaks",
                hoard_net::identity::MAX_NAME
            )));
        }
        let url = Url::parse(self.public_url.trim()).map_err(|_| {
            ApiError::bad_request("public URL must be an absolute HTTP or HTTPS address")
        })?;
        let usable_host = match url.host() {
            Some(Host::Domain(host)) => !host.is_empty() && host != "*",
            Some(Host::Ipv4(host)) => !host.is_unspecified(),
            Some(Host::Ipv6(host)) => !host.is_unspecified(),
            None => false,
        };
        if !matches!(url.scheme(), "http" | "https")
            || !usable_host
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || url.path() != "/"
        {
            return Err(ApiError::bad_request(
                "public URL must name an HTTP or HTTPS host without credentials, a path, query or fragment",
            ));
        }
        Ok(Self {
            node_name: node_name.to_owned(),
            public_url: url.as_str().trim_end_matches('/').to_owned(),
        })
    }
}

/// The `[node]` section.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeConfig {
    #[serde(default = "default_node_name")]
    pub name: String,
    /// Whether artifacts held here are offered on the LAN, which is the SERVED state.
    #[serde(default = "default_true")]
    pub serve: bool,
    #[serde(default)]
    pub discovery: Discovery,
}

impl Default for NodeConfig {
    fn default() -> Self {
        Self {
            name: default_node_name(),
            serve: true,
            discovery: Discovery::default(),
        }
    }
}

/// How peers are found. mDNS may be blocked on some networks, so a static list is the
/// documented fallback rather than an afterthought.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Discovery {
    #[default]
    Mdns,
    Static,
    None,
}

/// The `[peers]` section.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PeersConfig {
    #[serde(default, rename = "static")]
    pub statics: Vec<String>,
}

/// A whole `hoard.toml`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HoardConfig {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub node: NodeConfig,
    #[serde(default)]
    pub peers: PeersConfig,
    #[serde(default)]
    pub index: IndexConfig,
    #[serde(default)]
    pub mirror: MirrorConfig,
    #[serde(default)]
    pub lookup: LookupConfig,
}

impl HoardConfig {
    pub fn parse(text: &str) -> Result<Self, ApiError> {
        let mut config: HoardConfig = toml::from_str(text)
            .map_err(|error| ApiError::bad_request(format!("hoard.toml: {error}")))?;
        config.normalize(home_dir().as_deref());
        Ok(config)
    }

    pub fn load(path: &Path) -> Result<Self, ApiError> {
        let text = std::fs::read_to_string(path)
            .map_err(|error| ApiError::bad_request(format!("{}: {error}", path.display())))?;
        let mut config = Self::parse(&text)?;
        config.load_managed_sources()?;
        Ok(config)
    }

    /// Applies saved node settings, UI sources and persisted removals at startup. Managed
    /// settings override TOML without rewriting it; restoring a source overrides its name.
    pub fn load_managed_sources(&mut self) -> Result<(), ApiError> {
        if let Some(settings) = self.managed_node_settings()? {
            self.node.name = settings.node_name;
            self.server.public_url = Some(settings.public_url);
        }
        for source in self.managed_sources()? {
            if let Some(existing) = self
                .index
                .sources
                .iter_mut()
                .find(|held| held.name == source.name)
            {
                *existing = source;
            } else {
                self.index.sources.push(source);
            }
        }
        let disabled = self.disabled_sources()?;
        self.index
            .sources
            .retain(|source| !disabled.contains(&source.name));
        Ok(())
    }

    pub(crate) fn managed_node_settings(&self) -> Result<Option<NodeSettings>, ApiError> {
        read_source_file::<Option<NodeSettings>>(&self.server.data_dir.join("node.json"))?
            .map(NodeSettings::validated)
            .transpose()
    }

    pub(crate) fn save_node_settings(&self, settings: &NodeSettings) -> Result<(), ApiError> {
        write_source_file(&self.server.data_dir.join("node.json"), settings)
    }

    /// Persists one UI-created folder without rewriting the user's hoard.toml.
    pub fn append_managed_source(&self, source: &SourceConfig) -> Result<(), ApiError> {
        let path = self.managed_sources_path();
        let mut sources = self.managed_sources()?;
        if let Some(existing) = sources.iter_mut().find(|held| held.name == source.name) {
            *existing = source.clone();
        } else {
            sources.push(source.clone());
        }
        write_source_file(&path, &sources)?;
        let mut disabled = self.disabled_sources()?;
        if disabled.remove(&source.name) {
            write_source_file(&self.disabled_sources_path(), &disabled)?;
        }
        Ok(())
    }

    fn managed_sources(&self) -> Result<Vec<SourceConfig>, ApiError> {
        read_source_file(&self.managed_sources_path())
    }

    pub(crate) fn disabled_sources(&self) -> Result<BTreeSet<String>, ApiError> {
        read_source_file(&self.disabled_sources_path())
    }

    pub(crate) fn disable_source(&self, name: &str) -> Result<(), ApiError> {
        let mut disabled = self.disabled_sources()?;
        disabled.insert(name.to_owned());
        write_source_file(&self.disabled_sources_path(), &disabled)
    }

    fn disabled_sources_path(&self) -> PathBuf {
        self.server.data_dir.join("sources.disabled.json")
    }

    fn managed_sources_path(&self) -> PathBuf {
        self.server.data_dir.join("sources.json")
    }

    /// Expands `~`, then makes the data directory the single root every other path hangs
    /// off, so one `data_dir` setting moves the index and the mirror cache together.
    pub fn normalize(&mut self, home: Option<&Path>) {
        self.server.data_dir = hoard_index::config::expand_tilde(&self.server.data_dir, home);
        for root in &mut self.server.projection_roots {
            *root = hoard_index::config::expand_tilde(root, home);
        }
        if self.index.data_dir == default_data_dir() {
            self.index.data_dir = self.server.data_dir.clone();
        }
        if self.index.normalize(home).is_err() {
            tracing::warn!("duplicate source names in hoard.toml; later sources are ignored");
        }
        if self.mirror.cache_root.is_none() {
            self.mirror.cache_root = Some(self.server.data_dir.join("repos"));
        }
    }

    /// The mirror cache root once `data_dir` has been applied.
    pub fn cache_root(&self) -> PathBuf {
        self.mirror.resolved_cache_root(&self.server.data_dir)
    }
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

fn hostname() -> Option<String> {
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|raw| raw.trim().to_owned())
        .filter(|name| !name.is_empty())
}

fn read_source_file<T: serde::de::DeserializeOwned + Default>(path: &Path) -> Result<T, ApiError> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|error| ApiError::internal(format!("{}: {error}", path.display()))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(error) => Err(ApiError::internal(format!("{}: {error}", path.display()))),
    }
}

fn write_source_file(path: &Path, value: &impl Serialize) -> Result<(), ApiError> {
    let bytes =
        serde_json::to_vec_pretty(value).map_err(|error| ApiError::internal(error.to_string()))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| ApiError::internal(format!("{}: {error}", parent.display())))?;
    }
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, bytes)
        .map_err(|error| ApiError::internal(format!("{}: {error}", temporary.display())))?;
    std::fs::rename(&temporary, path)
        .map_err(|error| ApiError::internal(format!("{}: {error}", path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_example_config_parses() {
        let text = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../hoard.example.toml"
        ))
        .expect("the example config is committed");
        let config = HoardConfig::parse(&text).expect("the example config parses");
        assert_eq!(config.node.name, "workstation");
        assert_eq!(config.index.sources.len(), 3);
        assert_eq!(config.mirror.proxy.len(), 2);
        assert_eq!(config.mirror.cache.len(), 2);
        assert_eq!(config.mirror.cache_size_limit, Some(500_000_000_000));
        assert_eq!(config.mirror.geometry.block_size(), 64 * 1024 * 1024);
    }

    #[test]
    fn defaults_need_no_config_at_all() {
        let config = HoardConfig::parse("").expect("an empty config is valid");
        assert_eq!(config.server.bind, SocketAddr::from(([0, 0, 0, 0], 7099)));
        assert!(config.node.serve);
        assert!(config.index.sources.is_empty());
    }

    #[test]
    fn data_dir_becomes_the_root_for_the_catalog_and_the_cache() {
        let mut config =
            HoardConfig::parse("[server]\ndata_dir = \"/srv/hoard\"\n").expect("config parses");
        config.normalize(None);
        assert_eq!(
            config.index.native_path(),
            PathBuf::from("/srv/hoard/native")
        );
        assert_eq!(config.cache_root(), PathBuf::from("/srv/hoard/repos"));
    }

    #[test]
    fn a_projection_root_is_expanded_like_every_other_path() {
        let mut config = HoardConfig {
            server: ServerConfig {
                projection_roots: vec![PathBuf::from("~/ComfyUI/models")],
                ..ServerConfig::default()
            },
            ..HoardConfig::default()
        };
        config.normalize(Some(Path::new("/home/hoarder")));
        assert_eq!(
            config.server.projection_roots,
            vec![PathBuf::from("/home/hoarder/ComfyUI/models")]
        );
    }

    #[test]
    fn nothing_outside_the_data_directory_is_writable_by_default() {
        assert!(HoardConfig::default().server.projection_roots.is_empty());
    }

    #[test]
    fn the_lookup_is_off_until_a_section_asks_for_it() {
        let absent = HoardConfig::parse("[server]\n").expect("config parses");
        assert!(
            !absent.lookup.civitai,
            "a config with no lookup section sends nothing anywhere"
        );
        assert_eq!(absent.lookup.civitai_url, "https://civitai.com");

        let asked = HoardConfig::parse("[lookup]\ncivitai = true\n").expect("config parses");
        assert!(asked.lookup.civitai);
        assert_eq!(
            asked.lookup.civitai_url, "https://civitai.com",
            "the url a switched-on lookup uses is still the default one"
        );

        let elsewhere =
            HoardConfig::parse("[lookup]\ncivitai_url = \"http://127.0.0.1:9\"\n").expect("parses");
        assert!(!elsewhere.lookup.civitai);
        assert_eq!(elsewhere.lookup.civitai_url, "http://127.0.0.1:9");
    }

    #[test]
    fn public_url_uses_a_local_fallback_instead_of_an_unspecified_address() {
        let config = HoardConfig::default();
        assert_eq!(config.server.resolved_public_url(), "http://127.0.0.1:7099");
        assert_eq!(config.server.discovery_url(), "http://0.0.0.0:7099");
        let config = ServerConfig {
            bind: "[::]:7099".parse().expect("IPv6 address"),
            ..ServerConfig::default()
        };
        assert_eq!(config.resolved_public_url(), "http://[::1]:7099");
        assert_eq!(config.discovery_url(), "http://[::]:7099");
        let config = ServerConfig {
            bind: "192.0.2.4:7099".parse().expect("specific address"),
            ..ServerConfig::default()
        };
        assert_eq!(config.resolved_public_url(), "http://192.0.2.4:7099");
        assert_eq!(config.discovery_url(), "http://192.0.2.4:7099");
        let config = ServerConfig {
            public_url: Some("http://studio.lan:7099".to_owned()),
            ..ServerConfig::default()
        };
        assert_eq!(config.resolved_public_url(), "http://studio.lan:7099");
        assert_eq!(config.discovery_url(), "http://studio.lan:7099");
    }

    #[test]
    fn node_settings_reject_unusable_addresses_and_normalize_valid_input() {
        for public_url in [
            "http://0.0.0.0:7099",
            "http://[::]:7099",
            "http://*:7099",
            "file:///tmp",
            "http://user:password@hoard.lan:7099",
            "http://hoard.lan/path",
            "http://hoard.lan?token=x",
            "http://hoard.lan#page",
            "hoard.lan:7099",
        ] {
            let settings = NodeSettings {
                node_name: "Hoard".to_owned(),
                public_url: public_url.to_owned(),
            };
            assert!(settings.validated().is_err(), "{public_url}");
        }
        for node_name in ["", "   ", "Node\nName"] {
            let settings = NodeSettings {
                node_name: node_name.to_owned(),
                public_url: "http://hoard.lan".to_owned(),
            };
            assert!(settings.validated().is_err(), "{node_name:?}");
        }
        let mut settings = NodeSettings {
            node_name: "a".repeat(hoard_net::identity::MAX_NAME),
            public_url: "http://hoard.lan".to_owned(),
        };
        assert!(settings.clone().validated().is_ok());
        settings.node_name.push('a');
        assert!(settings.validated().is_err());
        let settings = NodeSettings {
            node_name: " Studio ".to_owned(),
            public_url: " https://hoard.lan:7099/ ".to_owned(),
        }
        .validated()
        .expect("valid settings");
        assert_eq!(settings.node_name, "Studio");
        assert_eq!(settings.public_url, "https://hoard.lan:7099");
    }

    #[test]
    fn saved_node_settings_override_toml_without_rewriting_it() {
        let directory = tempfile::tempdir().expect("temporary config");
        let path = directory.path().join("hoard.toml");
        let text = format!(
            "[server]\ndata_dir = {:?}\npublic_url = \"http://old.lan\"\n[node]\nname = \"old\"\n",
            directory.path()
        );
        std::fs::write(&path, &text).expect("config writes");
        let config = HoardConfig::load(&path).expect("config loads");
        assert!(
            config
                .managed_node_settings()
                .expect("settings read")
                .is_none()
        );
        let settings = NodeSettings {
            node_name: "Studio".to_owned(),
            public_url: "http://studio.lan:7099".to_owned(),
        };
        config.save_node_settings(&settings).expect("settings save");
        let reopened = HoardConfig::load(&path).expect("config reloads");
        assert_eq!(reopened.node.name, settings.node_name);
        assert_eq!(
            reopened.server.public_url.as_deref(),
            Some(settings.public_url.as_str())
        );
        assert_eq!(
            std::fs::read_to_string(path).expect("original config"),
            text
        );
    }
}
