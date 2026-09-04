use globset::{Glob, GlobBuilder, GlobMatcher};
use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::config::MirrorConfig;
use crate::error::{MirrorError, Result};

/// One entry of the `proxy` or `cache` rule list.
///
/// `repo` is matched against `org/repo` (no repo type). Glob rules follow olah's `fnmatch`
/// semantics, where `*` also crosses `/`; regex rules are anchored at the start only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccessRule {
    pub repo: String,
    #[serde(default)]
    pub allow: bool,
    #[serde(default)]
    pub use_re: bool,
}

impl AccessRule {
    pub fn glob(repo: impl Into<String>, allow: bool) -> Self {
        Self {
            repo: repo.into(),
            allow,
            use_re: false,
        }
    }

    pub fn regex(repo: impl Into<String>, allow: bool) -> Self {
        Self {
            repo: repo.into(),
            allow,
            use_re: true,
        }
    }
}

#[derive(Debug)]
enum Matcher {
    Glob(Box<GlobMatcher>),
    Regex(Box<Regex>),
}

impl Matcher {
    fn matches(&self, subject: &str) -> bool {
        match self {
            Matcher::Glob(glob) => glob.is_match(subject),
            Matcher::Regex(regex) => regex.is_match(subject),
        }
    }
}

#[derive(Debug)]
struct CompiledRule {
    matcher: Matcher,
    allow: bool,
}

/// One rule list with its globs and regexes compiled once.
///
/// The last matching rule wins and no match denies, exactly as olah's `OlahRuleList.allow`.
#[derive(Debug, Default)]
pub struct AccessRules {
    rules: Vec<CompiledRule>,
}

impl AccessRules {
    pub fn compile(rules: &[AccessRule]) -> Result<Self> {
        let mut compiled = Vec::with_capacity(rules.len());
        for rule in rules {
            let matcher = if rule.use_re {
                Matcher::Regex(Box::new(compile_anchored_regex(&rule.repo)?))
            } else {
                Matcher::Glob(Box::new(compile_fnmatch_glob(&rule.repo)?))
            };
            compiled.push(CompiledRule {
                matcher,
                allow: rule.allow,
            });
        }
        Ok(Self { rules: compiled })
    }

    pub fn allows(&self, subject: &str) -> bool {
        self.rules
            .iter()
            .rev()
            .find(|rule| rule.matcher.matches(subject))
            .is_some_and(|rule| rule.allow)
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }
}

/// The compiled `proxy` and `cache` rule lists of a `[mirror]` section.
#[derive(Debug, Default)]
pub struct AccessPolicy {
    proxy: AccessRules,
    cache: AccessRules,
}

impl AccessPolicy {
    pub fn compile(config: &MirrorConfig) -> Result<Self> {
        Ok(Self {
            proxy: AccessRules::compile(&config.proxy)?,
            cache: AccessRules::compile(&config.cache)?,
        })
    }

    /// Whether `org/repo` may be fetched from an upstream at all. A denial is 401
    /// `RepoNotFound`.
    pub fn proxy_allowed(&self, subject: &str) -> bool {
        self.proxy.allows(subject)
    }

    /// Whether a proxied response for `org/repo` may be persisted. A denial only disables
    /// persistence; the bytes still flow to the client.
    pub fn cache_allowed(&self, subject: &str) -> bool {
        self.cache.allows(subject)
    }
}

/// Compiles a glob with Python `fnmatch` semantics: the pattern must match the whole
/// subject, `*` and `?` cross `/`, and a backslash is a literal character rather than an
/// escape.
fn compile_fnmatch_glob(pattern: &str) -> Result<GlobMatcher> {
    let glob: Glob = GlobBuilder::new(pattern)
        .literal_separator(false)
        .backslash_escape(false)
        .build()
        .map_err(|error| MirrorError::Config(format!("bad rule glob {pattern:?}: {error}")))?;
    Ok(glob.compile_matcher())
}

/// Compiles a regex rule the way Python's `re.match` behaves: anchored at the start, free
/// at the end, with any top-level alternation kept inside the anchor.
fn compile_anchored_regex(pattern: &str) -> Result<Regex> {
    Regex::new(&format!("^(?:{pattern})"))
        .map_err(|error| MirrorError::Config(format!("bad rule regex {pattern:?}: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(rules: &[AccessRule]) -> AccessRules {
        AccessRules::compile(rules).expect("rules compile")
    }

    #[test]
    fn no_rule_matching_denies() {
        let list = rules(&[AccessRule::glob("openai/*", true)]);
        assert!(!list.allows("meta-llama/Llama-3"));
        assert!(!rules(&[]).allows("anything"));
    }

    #[test]
    fn the_last_matching_rule_wins() {
        let list = rules(&[
            AccessRule::glob("*", true),
            AccessRule::glob("secret/*", false),
            AccessRule::glob("secret/public", true),
        ]);
        assert!(list.allows("openai/whisper"));
        assert!(!list.allows("secret/weights"));
        assert!(list.allows("secret/public"));
    }

    #[test]
    fn a_star_crosses_the_slash_like_fnmatch() {
        let list = rules(&[AccessRule::glob("*", true)]);
        assert!(list.allows("org/repo"));
        assert!(list.allows("repo"));

        let prefixed = rules(&[AccessRule::glob("meta*", true)]);
        assert!(prefixed.allows("meta-llama/Llama-3-8B"));
    }

    #[test]
    fn a_regex_rule_is_anchored_at_the_start_only() {
        let list = rules(&[AccessRule::regex("openai/", true)]);
        assert!(list.allows("openai/whisper-large"));
        assert!(!list.allows("not-openai/whisper"));

        let alternation = rules(&[AccessRule::regex("openai|meta", true)]);
        assert!(alternation.allows("meta-llama/Llama-3"));
        assert!(!alternation.allows("x-meta/Llama-3"));
    }

    #[test]
    fn the_example_config_rule_set_allows_every_repo_form() {
        let config = MirrorConfig {
            proxy: vec![AccessRule::glob("*", true), AccessRule::glob("*/*", true)],
            cache: vec![AccessRule::glob("*", true), AccessRule::glob("*/*", true)],
            ..MirrorConfig::default()
        };
        let policy = AccessPolicy::compile(&config).expect("policy compiles");
        for subject in ["gpt2", "openai/whisper-large-v3", "org/name"] {
            assert!(policy.proxy_allowed(subject), "{subject}");
            assert!(policy.cache_allowed(subject), "{subject}");
        }
    }

    #[test]
    fn a_denied_cache_rule_still_allows_the_proxy() {
        let config = MirrorConfig {
            proxy: vec![AccessRule::glob("*/*", true)],
            cache: vec![
                AccessRule::glob("*/*", true),
                AccessRule::glob("private/*", false),
            ],
            ..MirrorConfig::default()
        };
        let policy = AccessPolicy::compile(&config).expect("policy compiles");
        assert!(policy.proxy_allowed("private/model"));
        assert!(!policy.cache_allowed("private/model"));
    }

    #[test]
    fn a_broken_pattern_is_a_config_error() {
        assert!(AccessRules::compile(&[AccessRule::regex("(", true)]).is_err());
        assert!(AccessRules::compile(&[AccessRule::glob("[", true)]).is_err());
    }
}
