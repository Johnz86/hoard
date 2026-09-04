//! Another hoard node as an upstream.
//!
//! A peer serves the same Hub face this crate implements, so a peer pull is an ordinary
//! block-cached resolve against `http://<peer>:7099`. Everything here is address
//! normalization; the transport is [`Hub`] unchanged.

use url::Url;

use crate::error::{MirrorError, Result};
use crate::upstream::client::UpstreamClient;
use crate::upstream::hub::Hub;

/// The port the design gives a hoard node.
pub const DEFAULT_PEER_PORT: u16 = 7099;

/// The base URL of a peer given as a host, a `host:port` or a full URL.
pub fn peer_base_url(address: &str) -> Result<Url> {
    let address = address.trim();
    if address.is_empty() {
        return Err(MirrorError::Config("peer address is empty".to_owned()));
    }
    let spelled = if address.contains("://") {
        address.to_owned()
    } else {
        format!("http://{address}")
    };
    let mut url = Url::parse(&spelled).map_err(|error| MirrorError::Config(error.to_string()))?;
    if url.host_str().is_none_or(str::is_empty) {
        return Err(MirrorError::Config(format!("peer {address} has no host")));
    }
    if url.port().is_none() && !address.contains("://") {
        url.set_port(Some(DEFAULT_PEER_PORT))
            .map_err(|()| MirrorError::Config(format!("peer {address} cannot carry a port")))?;
    }
    Ok(url)
}

/// The upstream that pulls from a peer, which is the same code path as `huggingface.co`
/// minus the caller's credentials: a peer is not the audience of an `hf_` token.
pub fn peer_upstream(client: UpstreamClient, address: &str) -> Result<Hub> {
    Hub::peer(client, peer_base_url(address)?.as_str())
}

#[cfg(test)]
mod tests {
    use hoard_core::{RepoId, RepoType};

    use super::*;

    fn client() -> UpstreamClient {
        UpstreamClient::new(false).expect("client builds")
    }

    #[test]
    fn a_bare_host_gets_the_node_port_and_plain_http() {
        assert_eq!(
            peer_base_url("hoard.lan").expect("url").as_str(),
            "http://hoard.lan:7099/"
        );
        assert_eq!(
            peer_base_url("192.168.1.4:9000").expect("url").as_str(),
            "http://192.168.1.4:9000/"
        );
        assert_eq!(
            peer_base_url("https://hoard.lan/").expect("url").as_str(),
            "https://hoard.lan/"
        );
    }

    #[test]
    fn an_unusable_address_is_a_config_error() {
        assert!(peer_base_url("").is_err());
        assert!(peer_base_url("   ").is_err());
        assert!(peer_base_url("http://").is_err());
    }

    #[test]
    fn a_peer_pull_asks_for_exactly_the_hub_routes() {
        let hub = peer_upstream(client(), "hoard.lan").expect("peer upstream builds");
        assert_eq!(hub.kind(), crate::upstream::hub::UpstreamKind::Peer);
        assert!(
            hub.forwarded_auth(Some(&crate::upstream::client::Authorization::new(
                "Bearer hf_secret"
            )))
            .is_none(),
            "a peer is not the audience of the caller's token"
        );
        let repo = RepoId::new(RepoType::Models, Some("openai".to_owned()), "whisper");
        assert_eq!(
            hub.api_url(&repo, "/revision/main").expect("url").as_str(),
            "http://hoard.lan:7099/api/models/openai/whisper/revision/main"
        );
        assert_eq!(
            hub.resolve_url(&repo, "main", "model.safetensors")
                .expect("url")
                .as_str(),
            "http://hoard.lan:7099/openai/whisper/resolve/main/model.safetensors"
        );
    }
}
