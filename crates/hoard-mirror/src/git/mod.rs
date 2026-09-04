//! The git face: read-only smart HTTP, the LFS batch API and the route shapes both use.

pub mod lfs_batch;
pub mod smart_http;

use hoard_core::clean_repo_path;

pub use lfs_batch::{
    BatchAction, BatchActions, BatchObject, BatchObjectError, BatchOperation, BatchRef,
    BatchRequest, BatchResponse, BatchResponseObject, LfsLocation, TransferAdapter,
    build_batch_response,
};
pub use smart_http::{
    ADVERTISEMENT_CONTENT_TYPE, CACHE_CONTROL, PUSH_REFUSED, RECEIVE_PACK_SERVICE,
    RESULT_CONTENT_TYPE, UPLOAD_PACK_SERVICE, UploadPack, advertise_refs, is_clonable, pkt_line,
    refuse_push, service_header, upload_pack,
};

const MAX_REFERENCE_SEGMENTS: usize = 2;
const GIT_SUFFIX: &str = ".git";
const ROUTE_SUFFIXES: [(&str, GitRoute); 4] = [
    ("info/lfs/objects/batch", GitRoute::LfsBatch),
    ("info/refs", GitRoute::AdvertiseRefs),
    ("git-upload-pack", GitRoute::UploadPack),
    ("git-receive-pack", GitRoute::ReceivePack),
];

/// The service named by the `service` query parameter of `info/refs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitService {
    UploadPack,
    ReceivePack,
}

impl GitService {
    pub const fn as_str(self) -> &'static str {
        match self {
            GitService::UploadPack => UPLOAD_PACK_SERVICE,
            GitService::ReceivePack => RECEIVE_PACK_SERVICE,
        }
    }

    pub fn parse(value: &str) -> Option<GitService> {
        match value {
            UPLOAD_PACK_SERVICE => Some(GitService::UploadPack),
            RECEIVE_PACK_SERVICE => Some(GitService::ReceivePack),
            _ => None,
        }
    }

    /// Fetch services are served; push services get the read-only refusal.
    pub const fn is_read_only(self) -> bool {
        matches!(self, GitService::UploadPack)
    }
}

/// Which git face endpoint a request path names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitRoute {
    AdvertiseRefs,
    UploadPack,
    ReceivePack,
    LfsBatch,
}

impl GitRoute {
    /// True for the routes hoard answers; `ReceivePack` is refused.
    pub const fn is_read_only(self) -> bool {
        !matches!(self, GitRoute::ReceivePack)
    }
}

/// A recognised git face request: the repo it names, with any `.git` suffix removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitRequest {
    pub reference: String,
    pub route: GitRoute,
}

/// Recognises `/{org}/{repo}[.git]/info/refs`, `/git-upload-pack`, `/git-receive-pack` and
/// `/info/lfs/objects/batch`, normalising the `.git` suffix away. Returns `None` for any
/// other shape, for more than two leading segments and for paths containing `..`.
pub fn parse_git_route(path: &str) -> Option<GitRequest> {
    let trimmed = path.trim_matches('/');
    for (suffix, route) in ROUTE_SUFFIXES {
        let Some(head) = trimmed
            .strip_suffix(suffix)
            .and_then(|head| head.strip_suffix('/'))
        else {
            continue;
        };
        return normalize_reference(head).map(|reference| GitRequest { reference, route });
    }
    None
}

/// Removes a trailing `.git` from a repo reference.
pub fn strip_git_suffix(reference: &str) -> &str {
    reference.strip_suffix(GIT_SUFFIX).unwrap_or(reference)
}

fn normalize_reference(head: &str) -> Option<String> {
    let cleaned = clean_repo_path(head)?;
    let reference = strip_git_suffix(&cleaned);
    if reference.is_empty() || reference.split('/').count() > MAX_REFERENCE_SEGMENTS {
        return None;
    }
    Some(reference.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognised_routes_normalise_the_git_suffix() {
        let cases = [
            (
                "/org/repo.git/info/refs",
                "org/repo",
                GitRoute::AdvertiseRefs,
            ),
            ("/org/repo/info/refs", "org/repo", GitRoute::AdvertiseRefs),
            ("/gpt2/info/refs", "gpt2", GitRoute::AdvertiseRefs),
            (
                "/org/repo.git/git-upload-pack",
                "org/repo",
                GitRoute::UploadPack,
            ),
            (
                "/org/repo/git-upload-pack",
                "org/repo",
                GitRoute::UploadPack,
            ),
            (
                "/org/repo.git/git-receive-pack",
                "org/repo",
                GitRoute::ReceivePack,
            ),
            (
                "org/repo.git/info/lfs/objects/batch",
                "org/repo",
                GitRoute::LfsBatch,
            ),
            (
                "/org/repo/info/lfs/objects/batch",
                "org/repo",
                GitRoute::LfsBatch,
            ),
            (
                "//org//repo.git//info/refs",
                "org/repo",
                GitRoute::AdvertiseRefs,
            ),
        ];
        for (path, reference, route) in cases {
            assert_eq!(
                parse_git_route(path),
                Some(GitRequest {
                    reference: reference.to_owned(),
                    route
                }),
                "{path}"
            );
        }
    }

    #[test]
    fn other_shapes_are_not_git_routes() {
        let cases = [
            "/info/refs",
            "/org/repo/resolve/main/info/refs",
            "/api/models/org/repo",
            "/org/repo/info/lfs/objects/verify",
            "/org/repo/info/lfsobjects/batch",
            "/org/repoinfo/refs",
            "/org/repo/git-upload-packet",
            "/../repo/info/refs",
            "/org/../../etc/info/refs",
            "/.git/info/refs",
            "/org/repo/",
            "",
        ];
        for path in cases {
            assert_eq!(parse_git_route(path), None, "{path}");
        }
    }

    #[test]
    fn only_upload_pack_is_served() {
        assert_eq!(
            GitService::parse("git-upload-pack"),
            Some(GitService::UploadPack)
        );
        assert_eq!(
            GitService::parse("git-receive-pack"),
            Some(GitService::ReceivePack)
        );
        assert_eq!(GitService::parse("git-archive"), None);
        assert_eq!(GitService::UploadPack.as_str(), "git-upload-pack");
        assert!(GitService::UploadPack.is_read_only());
        assert!(!GitService::ReceivePack.is_read_only());
        assert!(GitRoute::LfsBatch.is_read_only());
        assert!(!GitRoute::ReceivePack.is_read_only());
    }

    #[test]
    fn git_suffix_is_stripped_once() {
        assert_eq!(strip_git_suffix("org/repo.git"), "org/repo");
        assert_eq!(strip_git_suffix("org/repo"), "org/repo");
        assert_eq!(strip_git_suffix("org.git/repo"), "org.git/repo");
        assert_eq!(strip_git_suffix(".git"), "");
    }
}
