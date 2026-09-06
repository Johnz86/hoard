pub mod apply;
pub mod catalog;
pub mod doctor;
pub mod link;
pub mod loadout;
pub mod ls;
pub mod node;
pub mod path;
pub mod scan;
pub mod serve;

use std::path::PathBuf;

use clap::{Parser, Subcommand};

use hoard_index::config::HashPolicy;

/// Local HuggingFace-compatible mirror and artifact index.
#[derive(Debug, Parser)]
#[command(name = "hoard", version, about, long_about = None)]
pub struct Cli {
    /// Path to `hoard.toml`. Defaults to `./hoard.toml` when it exists.
    #[arg(long, short, global = true, env = "HOARD_CONFIG")]
    pub config: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Maintain the native catalog: scan into a directory, search it, report status or
    /// rebuild its search index. One process at a time.
    Catalog {
        #[command(subcommand)]
        command: catalog::CatalogCommand,
    },
    /// Serve the Hub API, the git face and the web UI on this node.
    Serve {
        /// Override `[server] bind`.
        #[arg(long)]
        bind: Option<String>,
    },
    /// Index the nominated sources.
    Scan {
        /// Only this source, by name.
        #[arg(long)]
        source: Option<String>,
        /// When to compute sha256. Overrides `[index] hash_policy` for this run.
        #[arg(long, value_enum)]
        hash: Option<HashWhen>,
    },
    /// Search the index with the query grammar.
    #[command(alias = "search")]
    Ls {
        /// The query, in the same grammar the search bar uses.
        query: Option<String>,
        #[arg(long)]
        json: bool,
        #[arg(long, default_value_t = 50)]
        limit: u32,
        /// Sort column: role, name, base, format, precision, size, revision, node, state, used.
        #[arg(long, default_value = "size")]
        sort: String,
        #[arg(long)]
        asc: bool,
    },
    /// Print the on-disk path of a repo or artifact.
    Path {
        /// `org/name`, `type/org/name`, an artifact id or a sha256.
        target: String,
    },
    /// Work with loadout documents.
    Loadout {
        #[command(subcommand)]
        command: loadout::LoadoutCommand,
    },
    /// Resolve a loadout on a node, pulling what is missing and writing symlinks.
    Apply {
        /// Loadout name or path to a TOML document.
        loadout: String,
        /// The node to apply it to. Defaults to this one.
        #[arg(long)]
        node: Option<String>,
        /// Tool layout to write.
        #[arg(long, default_value = "comfyui")]
        profile: String,
    },
    /// Project a query or shelf into a tool's directory layout as symlinks.
    Link {
        /// Destination directory.
        dir: PathBuf,
        #[arg(long, default_value = "comfyui")]
        profile: String,
        /// The query to project.
        #[arg(long)]
        query: Option<String>,
        /// A saved shelf to project instead.
        #[arg(long)]
        shelf: Option<String>,
    },
    /// List this node and its peers.
    Nodes,
    /// Check that this installation can do everything it claims.
    Doctor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum HashWhen {
    /// Hash during the scan, before it reports done.
    Now,
    /// Hash afterwards in the background pool. The default `[index] hash_policy`.
    Later,
    /// Never hash; `resolve` of a loose file stays 404 because clients verify ETags.
    Never,
}

impl From<HashWhen> for HashPolicy {
    fn from(value: HashWhen) -> Self {
        match value {
            HashWhen::Now => HashPolicy::Now,
            HashWhen::Later => HashPolicy::Background,
            HashWhen::Never => HashPolicy::Never,
        }
    }
}

/// The policy a scan runs under: the `--hash` flag when it was given, otherwise the
/// `[index] hash_policy` the configuration carries.
pub fn hash_policy(flag: Option<HashWhen>, configured: HashPolicy) -> HashPolicy {
    flag.map_or(configured, HashPolicy::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn the_command_tree_is_well_formed() {
        Cli::command().debug_assert();
    }

    #[test]
    fn every_documented_invocation_parses() {
        let cases: [&[&str]; 9] = [
            &["hoard", "serve", "--config", "hoard.toml"],
            &["hoard", "serve", "--bind", "0.0.0.0:7099"],
            &["hoard", "scan", "--source", "comfy", "--hash", "now"],
            &["hoard", "ls", "role:lora base:sdxl", "--json"],
            &["hoard", "path", "org/name"],
            &["hoard", "loadout", "validate", "night.toml"],
            &["hoard", "apply", "night-exterior-v3", "--node", "laptop"],
            &["hoard", "link", "--profile", "comfyui", "./models"],
            &["hoard", "doctor"],
        ];
        for case in cases {
            Cli::try_parse_from(case).unwrap_or_else(|error| panic!("{case:?}: {error}"));
        }
    }

    #[test]
    fn search_is_an_alias_for_ls() {
        let parsed = Cli::try_parse_from(["hoard", "search", "role:llm"]).expect("alias parses");
        assert!(matches!(parsed.command, Command::Ls { .. }));
    }

    #[test]
    fn the_hash_flag_is_optional_and_wins_over_the_config() {
        let bare = Cli::try_parse_from(["hoard", "scan"]).expect("a bare scan parses");
        assert!(matches!(bare.command, Command::Scan { hash: None, .. }));

        let flagged =
            Cli::try_parse_from(["hoard", "scan", "--hash", "never"]).expect("--hash never parses");
        assert!(matches!(
            flagged.command,
            Command::Scan {
                hash: Some(HashWhen::Never),
                ..
            }
        ));

        assert_eq!(hash_policy(None, HashPolicy::Never), HashPolicy::Never);
        assert_eq!(
            hash_policy(None, HashPolicy::Background),
            HashPolicy::Background
        );
        assert_eq!(
            hash_policy(Some(HashWhen::Now), HashPolicy::Never),
            HashPolicy::Now
        );
    }

    #[test]
    fn hash_spellings_map_onto_the_index_policy() {
        assert_eq!(HashPolicy::from(HashWhen::Now), HashPolicy::Now);
        assert_eq!(HashPolicy::from(HashWhen::Later), HashPolicy::Background);
        assert_eq!(HashPolicy::from(HashWhen::Never), HashPolicy::Never);
    }
}
