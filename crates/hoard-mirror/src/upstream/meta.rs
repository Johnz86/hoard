//! The metadata routes hoard forwards rather than answers: which document a request names,
//! where that document is cached and what the one upstream call for it looks like.
//!
//! One request produces one upstream call. The commit a document is filed under comes from
//! the answer itself, never from a second lookup.

use std::path::PathBuf;

use hoard_core::{CommitSha, RepoId};
use reqwest::Method;

use crate::cache::CacheLayout;
use crate::error::Result;
use crate::upstream::client::{Authorization, RequestBody};

/// The revision every metadata route without one means.
pub const DEFAULT_REVISION: &str = "main";

/// Which metadata document a Hub route names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetaKind<'a> {
    /// `/api/{repo_type}/{ref}`, and `/api/{repo_type}/{ref}/revision/{revision}` when the
    /// route named one.
    RepoInfo {
        revision: Option<&'a str>,
    },
    Tree {
        revision: &'a str,
        path: &'a str,
        recursive: bool,
        expand: bool,
    },
    PathsInfo {
        revision: &'a str,
        paths: &'a [String],
    },
    Commits {
        revision: &'a str,
    },
}

impl<'a> MetaKind<'a> {
    /// The revision the route resolves against, which is the default branch when it named
    /// none.
    pub fn revision(&self) -> &'a str {
        match self {
            MetaKind::RepoInfo { revision } => revision.unwrap_or(DEFAULT_REVISION),
            MetaKind::Tree { revision, .. }
            | MetaKind::PathsInfo { revision, .. }
            | MetaKind::Commits { revision } => revision,
        }
    }

    pub fn method(&self) -> Method {
        match self {
            MetaKind::PathsInfo { .. } => Method::POST,
            MetaKind::RepoInfo { .. } | MetaKind::Tree { .. } | MetaKind::Commits { .. } => {
                Method::GET
            }
        }
    }

    /// What follows `/api/{repo_type}/{ref}` in the upstream URL.
    pub fn suffix(&self) -> String {
        match self {
            MetaKind::RepoInfo { revision: None } => String::new(),
            MetaKind::RepoInfo {
                revision: Some(revision),
            } => format!("/revision/{revision}"),
            MetaKind::Tree {
                revision, path: "", ..
            } => format!("/tree/{revision}"),
            MetaKind::Tree { revision, path, .. } => format!("/tree/{revision}/{path}"),
            MetaKind::PathsInfo { revision, .. } => format!("/paths-info/{revision}"),
            MetaKind::Commits { revision } => format!("/commits/{revision}"),
        }
    }

    /// The query string the upstream call carries, if the route has one.
    pub fn query(&self) -> Option<String> {
        match self {
            MetaKind::Tree {
                recursive, expand, ..
            } => Some(format!("recursive={recursive}&expand={expand}")),
            MetaKind::RepoInfo { .. } | MetaKind::PathsInfo { .. } | MetaKind::Commits { .. } => {
                None
            }
        }
    }

    /// The body the upstream call carries, spelled as the Hub document rather than as
    /// whichever of the two accepted forms the client used.
    pub fn body(&self) -> Result<Option<RequestBody>> {
        match self {
            MetaKind::PathsInfo { paths, .. } => {
                let document = serde_json::json!({ "paths": paths });
                Ok(Some(RequestBody::json(serde_json::to_vec(&document)?)))
            }
            MetaKind::RepoInfo { .. } | MetaKind::Tree { .. } | MetaKind::Commits { .. } => {
                Ok(None)
            }
        }
    }

    /// Where the answer is cached, one document per path for the routes whose layout is
    /// keyed by path.
    pub fn documents(
        &self,
        layout: &CacheLayout,
        repo: &RepoId,
        sha: &CommitSha,
    ) -> Result<Vec<PathBuf>> {
        match self {
            MetaKind::RepoInfo { .. } => Ok(vec![layout.revision_meta(repo, sha)?]),
            MetaKind::Tree {
                path,
                recursive,
                expand,
                ..
            } => Ok(vec![layout.tree(repo, sha, path, *recursive, *expand)?]),
            MetaKind::PathsInfo { paths, .. } => paths
                .iter()
                .map(|path| layout.paths_info(repo, sha, path))
                .collect(),
            MetaKind::Commits { .. } => Ok(vec![layout.commits(repo, sha)?]),
        }
    }

    /// Whether the answer is an array assembled from one document per path.
    pub const fn is_split_by_path(&self) -> bool {
        matches!(self, MetaKind::PathsInfo { .. })
    }
}

/// One metadata request: the repository, the document and the caller's credentials.
#[derive(Debug, Clone, Copy)]
pub struct MetaRequest<'a> {
    pub repo: &'a RepoId,
    pub kind: MetaKind<'a>,
    pub auth: Option<&'a Authorization>,
}

impl<'a> MetaRequest<'a> {
    pub fn new(repo: &'a RepoId, kind: MetaKind<'a>) -> Self {
        Self {
            repo,
            kind,
            auth: None,
        }
    }

    pub fn with_auth(mut self, auth: Option<&'a Authorization>) -> Self {
        self.auth = auth;
        self
    }

    pub fn revision(&self) -> &'a str {
        self.kind.revision()
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use hoard_core::RepoType;

    use super::*;

    const SHA: &str = "9f1a2b3c4d5e6f708192a3b4c5d6e7f809a1b2c3";

    fn repo() -> RepoId {
        RepoId::new(RepoType::Models, Some("openai".to_owned()), "whisper")
    }

    fn rendered(kind: MetaKind<'_>) -> String {
        let layout = CacheLayout::new("/data/repos");
        let sha = CommitSha::from_str(SHA).expect("sha parses");
        let documents = kind
            .documents(&layout, &repo(), &sha)
            .expect("documents build");
        documents
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[test]
    fn every_route_names_its_upstream_url_and_its_document() {
        let prefix = "/data/repos/api/models/openai/whisper";

        let bare = MetaKind::RepoInfo { revision: None };
        assert_eq!(bare.suffix(), "");
        assert_eq!(bare.revision(), DEFAULT_REVISION);
        assert_eq!(
            rendered(bare),
            format!("{prefix}/revision/{SHA}/meta_get.json")
        );

        let named = MetaKind::RepoInfo {
            revision: Some("v1.0"),
        };
        assert_eq!(named.suffix(), "/revision/v1.0");
        assert_eq!(named.revision(), "v1.0");

        let tree = MetaKind::Tree {
            revision: "main",
            path: "unet",
            recursive: true,
            expand: false,
        };
        assert_eq!(tree.suffix(), "/tree/main/unet");
        assert_eq!(tree.query().as_deref(), Some("recursive=true&expand=false"));
        assert_eq!(
            rendered(tree),
            format!("{prefix}/tree/{SHA}/unet/tree_get_recursive_True_expand_False.json")
        );

        let root = MetaKind::Tree {
            revision: "main",
            path: "",
            recursive: false,
            expand: true,
        };
        assert_eq!(root.suffix(), "/tree/main");
        assert_eq!(
            rendered(root),
            format!("{prefix}/tree/{SHA}/tree_get_recursive_False_expand_True.json")
        );

        let commits = MetaKind::Commits { revision: "main" };
        assert_eq!(commits.suffix(), "/commits/main");
        assert_eq!(
            rendered(commits),
            format!("{prefix}/commits/{SHA}/commits_get.json")
        );
    }

    #[test]
    fn paths_info_posts_the_hub_document_and_files_one_answer_per_path() {
        let paths = vec!["config.json".to_owned(), "unet/model.bin".to_owned()];
        let kind = MetaKind::PathsInfo {
            revision: "main",
            paths: &paths,
        };
        assert_eq!(kind.method(), Method::POST);
        assert_eq!(kind.suffix(), "/paths-info/main");
        assert!(kind.is_split_by_path());

        let body = kind
            .body()
            .expect("body builds")
            .expect("a post has a body");
        assert_eq!(body.content_type, "application/json");
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body.bytes).expect("json"),
            serde_json::json!({ "paths": ["config.json", "unet/model.bin"] })
        );

        let prefix = "/data/repos/api/models/openai/whisper/paths-info";
        assert_eq!(
            rendered(kind),
            format!(
                "{prefix}/{SHA}/config.json/paths-info_post.json \
                 {prefix}/{SHA}/unet/model.bin/paths-info_post.json"
            )
        );
    }

    #[test]
    fn a_get_route_carries_no_body_and_only_the_tree_carries_a_query() {
        for kind in [
            MetaKind::RepoInfo { revision: None },
            MetaKind::Commits { revision: "main" },
        ] {
            assert_eq!(kind.method(), Method::GET);
            assert!(kind.body().expect("body builds").is_none());
            assert_eq!(kind.query(), None);
            assert!(!kind.is_split_by_path());
        }
    }
}
