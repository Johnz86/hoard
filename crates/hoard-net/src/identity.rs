//! What a node advertises about itself.
//!
//! The identity is a value: an id, a name and the URL other machines reach this node on,
//! plus the `_hoard._tcp` TXT record those three travel in. Nothing here touches the
//! network or the index.

use std::net::IpAddr;

use hoard_core::NodeId;
use url::{Host, Url};

use crate::error::{NetError, Result};

/// The DNS-SD service type every hoard node registers and browses.
pub const SERVICE_TYPE: &str = "_hoard._tcp.local.";

/// The default port a peer is assumed to serve on when an address names none.
pub const DEFAULT_PORT: u16 = 7099;

/// TXT record keys. DNS-SD keys are case insensitive and conventionally short.
pub const TXT_ID: &str = "id";
pub const TXT_NAME: &str = "name";
pub const TXT_URL: &str = "url";
pub const TXT_VERSION: &str = "ver";

/// The version this node advertises, so a peer can refuse a digest it cannot read.
pub const PROTOCOL_VERSION: &str = "1";

/// The longest node name accepted from the network, since a name becomes a row in the NODES
/// tab and a value of the `node:` facet.
pub const MAX_NAME: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeIdentity {
    id: NodeId,
    name: String,
    url: Url,
    version: String,
}

impl NodeIdentity {
    /// Builds an identity from the `[node]` section and the resolved public URL.
    ///
    /// The URL is normalized the way a peer address is, so `rig-02`, `rig-02:7099` and
    /// `http://rig-02:7099/` are one node rather than three, and a bind address that names
    /// no interface is replaced by this machine's host name: `http://0.0.0.0:7099` tells a
    /// peer to connect to itself.
    pub fn new(id: NodeId, name: impl Into<String>, url: &str) -> Result<Self> {
        let name = clean_name(&name.into())
            .ok_or_else(|| NetError::Config("a node needs a name".to_owned()))?;
        Ok(Self {
            id,
            name,
            url: routable(normalize_address(url)?)?,
            version: PROTOCOL_VERSION.to_owned(),
        })
    }

    pub fn id(&self) -> &NodeId {
        &self.id
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn url(&self) -> &Url {
        &self.url
    }

    pub fn version(&self) -> &str {
        &self.version
    }

    /// The host a peer connects to. An unspecified bind address is useless in a TXT record,
    /// so a node that serves on `0.0.0.0` advertises its hostname instead.
    pub fn host(&self) -> &str {
        self.url.host_str().unwrap_or("localhost")
    }

    pub fn port(&self) -> u16 {
        self.url.port_or_known_default().unwrap_or(DEFAULT_PORT)
    }

    /// The DNS-SD instance name: the node name folded to the label alphabet, with the port
    /// appended so two nodes of the same name on one machine do not collide.
    pub fn instance_name(&self) -> String {
        format!("{}-{}", fold_label(&self.name), self.port())
    }

    /// The mDNS host name this node answers to, which must end in the local domain.
    pub fn host_name(&self) -> String {
        format!("{}.local.", fold_label(&self.name))
    }

    /// The TXT record, in the order a reader of a packet capture would want it.
    pub fn txt_records(&self) -> Vec<(String, String)> {
        vec![
            (TXT_ID.to_owned(), self.id.to_string()),
            (TXT_NAME.to_owned(), self.name.clone()),
            (TXT_URL.to_owned(), self.url.to_string()),
            (TXT_VERSION.to_owned(), self.version.clone()),
        ]
    }

    /// Reads an identity back out of a TXT record.
    ///
    /// `fallback_url` is what the browser resolved the instance to, used when the peer
    /// advertised no URL of its own or advertised one that leads back to the reader;
    /// without either there is nothing to connect to.
    pub fn from_txt(records: &[(String, String)], fallback_url: Option<&str>) -> Result<Self> {
        let value = |wanted: &str| {
            records
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case(wanted))
                .map(|(_, value)| value.trim())
                .filter(|value| !value.is_empty())
        };
        let advertised = value(TXT_URL)
            .map(normalize_address)
            .transpose()?
            .filter(|url| !reaches_the_reader(url));
        let url = match advertised {
            Some(url) => url,
            None => normalize_address(fallback_url.ok_or_else(|| {
                NetError::Discovery("a peer advertised no address of its own".to_owned())
            })?)?,
        };
        let name = value(TXT_NAME)
            .and_then(clean_name)
            .unwrap_or_else(|| url.host_str().unwrap_or("peer").to_owned());
        let id = match value(TXT_ID) {
            Some(raw) => {
                NodeId::new(raw).map_err(|error| NetError::Discovery(error.to_string()))?
            }
            None => address_identity(&url)?,
        };
        Ok(Self {
            id,
            name,
            url,
            version: value(TXT_VERSION).unwrap_or(PROTOCOL_VERSION).to_owned(),
        })
    }

    /// The identity a statically configured peer starts with, before it answers for itself.
    pub fn from_address(address: &str) -> Result<Self> {
        let url = normalize_address(address)?;
        let name =
            clean_name(url.host_str().unwrap_or("peer")).unwrap_or_else(|| "peer".to_owned());
        Ok(Self {
            id: address_identity(&url)?,
            name,
            url,
            version: PROTOCOL_VERSION.to_owned(),
        })
    }

    /// Replaces the id with one derived from the address.
    ///
    /// Two nodes started from one machine share a machine id, so a peer that advertises the
    /// id this node already calls its own is recorded under its address instead of merging
    /// into the local row.
    pub fn disambiguated_from(mut self, local: &NodeId) -> Result<Self> {
        if &self.id == local {
            self.id = address_identity(&self.url)?;
        }
        Ok(self)
    }
}

/// A host, a `host:port` or a full URL as one absolute base URL with a port.
pub fn normalize_address(address: &str) -> Result<Url> {
    let address = address.trim();
    if address.is_empty() {
        return Err(NetError::Config("a peer address is empty".to_owned()));
    }
    let spelled = match address.contains("://") {
        true => address.to_owned(),
        false => format!("http://{address}"),
    };
    let mut url = Url::parse(&spelled).map_err(|error| NetError::Config(error.to_string()))?;
    if url.host_str().is_none_or(str::is_empty) {
        return Err(NetError::Config(format!("peer {address} names no host")));
    }
    if url.port().is_none() && url.scheme() == "http" && url.set_port(Some(DEFAULT_PORT)).is_err() {
        return Err(NetError::Config(format!(
            "peer {address} cannot carry a port"
        )));
    }
    url.set_path("");
    url.set_query(None);
    url.set_fragment(None);
    Ok(url)
}

fn routable(mut url: Url) -> Result<Url> {
    if !is_unspecified(&url) {
        return Ok(url);
    }
    let host = local_host_name();
    url.set_host(Some(&host))
        .map_err(|error| NetError::Config(format!("host {host:?}: {error}")))?;
    Ok(url)
}

fn reaches_the_reader(url: &Url) -> bool {
    is_unspecified(url)
        || matches!(
            url.host(),
            Some(Host::Ipv4(address)) if address.is_loopback())
        || matches!(
            url.host(),
            Some(Host::Ipv6(address)) if address.is_loopback())
        || url.host_str() == Some("localhost")
}

fn is_unspecified(url: &Url) -> bool {
    match url.host() {
        Some(Host::Ipv4(address)) => address.is_unspecified(),
        Some(Host::Ipv6(address)) => address.is_unspecified(),
        Some(Host::Domain(_)) | None => false,
    }
}

fn local_host_name() -> String {
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .or_else(|| std::env::var("HOSTNAME").ok())
        .map(|raw| raw.trim().to_owned())
        .filter(|name| !name.is_empty() && name.parse::<IpAddr>().is_err())
        .unwrap_or_else(|| "localhost".to_owned())
}

/// A name fit to become a row in the NODES tab: no control characters, no runaway length.
pub fn clean_name(raw: &str) -> Option<String> {
    let cleaned: String = raw
        .trim()
        .chars()
        .filter(|letter| !letter.is_control())
        .take(MAX_NAME)
        .collect();
    let cleaned = cleaned.trim().to_owned();
    (!cleaned.is_empty()).then_some(cleaned)
}

/// A stable identity for a peer known only by where it answers.
pub fn address_identity(url: &Url) -> Result<NodeId> {
    let host = url.host_str().unwrap_or("peer");
    let port = url.port_or_known_default().unwrap_or(DEFAULT_PORT);
    NodeId::new(fold_label(&format!("{host}-{port}")))
        .map_err(|error| NetError::Config(error.to_string()))
}

fn fold_label(raw: &str) -> String {
    let folded: String = raw
        .chars()
        .map(|letter| match letter.is_ascii_alphanumeric() {
            true => letter.to_ascii_lowercase(),
            false => '-',
        })
        .collect();
    match folded.trim_matches('-') {
        "" => "hoard".to_owned(),
        trimmed => trimmed.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity() -> NodeIdentity {
        NodeIdentity::new(
            NodeId::new("workstation-1").expect("node id"),
            "workstation",
            "http://192.168.1.4:7099",
        )
        .expect("identity builds")
    }

    #[test]
    fn an_address_normalizes_to_a_base_url_with_a_port() {
        assert_eq!(
            normalize_address("hoard.lan").expect("url").as_str(),
            "http://hoard.lan:7099/"
        );
        assert_eq!(
            normalize_address("192.168.1.4:9000/ignored")
                .expect("url")
                .as_str(),
            "http://192.168.1.4:9000/"
        );
        assert_eq!(
            normalize_address("https://hoard.lan/")
                .expect("url")
                .as_str(),
            "https://hoard.lan/"
        );
        assert!(normalize_address("").is_err());
        assert!(normalize_address("http://").is_err());
    }

    #[test]
    fn the_txt_record_round_trips_the_identity() {
        let advertised = identity();
        let records = advertised.txt_records();
        let read = NodeIdentity::from_txt(&records, None).expect("txt reads");
        assert_eq!(read, advertised);
        assert_eq!(read.host(), "192.168.1.4");
        assert_eq!(read.port(), 7099);
    }

    #[test]
    fn a_txt_record_without_a_url_falls_back_to_the_resolved_address() {
        let records = vec![(TXT_NAME.to_owned(), "laptop".to_owned())];
        let read =
            NodeIdentity::from_txt(&records, Some("192.168.1.9:7099")).expect("fallback reads");
        assert_eq!(read.name(), "laptop");
        assert_eq!(read.url().as_str(), "http://192.168.1.9:7099/");
        assert_eq!(read.id().as_str(), "192-168-1-9-7099");
        assert!(NodeIdentity::from_txt(&records, None).is_err());
    }

    #[test]
    fn a_static_peer_is_identified_by_where_it_answers() {
        let peer = NodeIdentity::from_address("laptop.lan").expect("address identity");
        assert_eq!(peer.id().as_str(), "laptop-lan-7099");
        assert_eq!(peer.name(), "laptop.lan");
        assert_eq!(peer.url().as_str(), "http://laptop.lan:7099/");
    }

    #[test]
    fn a_peer_that_claims_the_local_id_is_recorded_under_its_address() {
        let local = NodeId::new("workstation-1").expect("node id");
        let twin = NodeIdentity::new(local.clone(), "second", "http://127.0.0.1:7100")
            .expect("identity builds")
            .disambiguated_from(&local)
            .expect("disambiguates");
        assert_eq!(twin.id().as_str(), "127-0-0-1-7100");

        let other = NodeId::new("laptop-1").expect("node id");
        let kept = NodeIdentity::new(other.clone(), "laptop", "http://laptop:7099")
            .expect("identity builds")
            .disambiguated_from(&local)
            .expect("keeps its own id");
        assert_eq!(kept.id(), &other);
    }

    #[test]
    fn instance_and_host_names_are_dns_labels() {
        let advertised = identity();
        assert_eq!(advertised.instance_name(), "workstation-7099");
        assert_eq!(advertised.host_name(), "workstation.local.");
        let awkward = NodeIdentity::new(
            NodeId::new("rig-02").expect("node id"),
            "Rig 02 (attic)",
            "http://rig:7099",
        )
        .expect("identity builds");
        assert_eq!(awkward.instance_name(), "rig-02--attic-7099");
    }

    #[test]
    fn a_node_that_binds_every_interface_advertises_a_host_a_peer_can_dial() {
        let bound = NodeIdentity::new(
            NodeId::new("workstation-1").expect("node id"),
            "workstation",
            "http://0.0.0.0:7099",
        )
        .expect("identity builds");
        assert_ne!(bound.host(), "0.0.0.0");
        assert_eq!(bound.host(), local_host_name());
        assert_eq!(bound.port(), 7099);

        let six = NodeIdentity::new(
            NodeId::new("workstation-1").expect("node id"),
            "workstation",
            "http://[::]:7099",
        )
        .expect("identity builds");
        assert_eq!(six.host(), local_host_name());
    }

    #[test]
    fn an_advertised_url_that_leads_back_to_the_reader_is_not_an_address() {
        let records = vec![
            (TXT_NAME.to_owned(), "laptop".to_owned()),
            (TXT_URL.to_owned(), "http://0.0.0.0:7099/".to_owned()),
        ];
        let read = NodeIdentity::from_txt(&records, Some("192.168.1.9:7099"))
            .expect("the resolved address stands in");
        assert_eq!(read.url().as_str(), "http://192.168.1.9:7099/");

        let loopback = vec![(TXT_URL.to_owned(), "http://127.0.0.1:7099/".to_owned())];
        assert_eq!(
            NodeIdentity::from_txt(&loopback, Some("192.168.1.9:7099"))
                .expect("the resolved address stands in")
                .url()
                .as_str(),
            "http://192.168.1.9:7099/"
        );
        assert!(NodeIdentity::from_txt(&loopback, None).is_err());
        assert!(
            NodeIdentity::from_txt(
                &[(TXT_URL.to_owned(), "http://localhost:7099".to_owned())],
                None
            )
            .is_err()
        );
    }

    #[test]
    fn a_name_from_the_network_is_trimmed_of_control_characters_and_bounded() {
        let records = vec![
            (
                TXT_NAME.to_owned(),
                format!("  a\u{1}b{}  ", "x".repeat(200)),
            ),
            (TXT_URL.to_owned(), "http://192.168.1.9:7099".to_owned()),
        ];
        let read = NodeIdentity::from_txt(&records, None).expect("txt reads");
        assert_eq!(read.name().chars().count(), MAX_NAME);
        assert!(read.name().starts_with("ab"));
        assert!(!read.name().chars().any(char::is_control));
        assert_eq!(clean_name("  \u{7}  "), None);
    }

    #[test]
    fn a_node_needs_a_name() {
        assert!(
            NodeIdentity::new(
                NodeId::new("workstation-1").expect("node id"),
                "   ",
                "http://rig:7099"
            )
            .is_err()
        );
    }
}
