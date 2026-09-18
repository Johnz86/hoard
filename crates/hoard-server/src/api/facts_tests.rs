use hoard_core::Role;
use hoard_index::catalog::Listed;
use hoard_index::row::SearchRow;

use super::common::{Fact, facts, listing_fact};
use crate::testing::{RowFacts, search_row};

fn files_fact(row: &SearchRow) -> Option<String> {
    facts(row, None)
        .into_iter()
        .find(|fact| fact.key == "files")
        .map(|fact| fact.value)
}

fn listed(model: &str, version: &str) -> Listed {
    Listed {
        base_model: "Illustrious".to_owned(),
        model: model.to_owned(),
        version: version.to_owned(),
        kind: "Checkpoint".to_owned(),
        trained_words: Vec::new(),
    }
}

#[test]
fn an_incomplete_sets_files_fact_names_its_total() {
    let lone = SearchRow {
        shard_count: 1,
        incomplete: true,
        shard_total: Some(14),
        ..search_row(RowFacts::default())
    };
    let undeclared = SearchRow {
        shard_total: None,
        ..lone.clone()
    };
    let whole = SearchRow {
        shard_count: 4,
        incomplete: false,
        shard_total: Some(4),
        ..search_row(RowFacts::default())
    };
    let single = search_row(RowFacts::default());

    assert_eq!(
        files_fact(&lone).as_deref(),
        Some("1 of 14 shards, incomplete"),
        "a set says how many of the files it declares are here"
    );
    assert_eq!(
        files_fact(&undeclared).as_deref(),
        Some("1 shards, incomplete"),
        "a set that declared no total still says it is short"
    );
    assert_eq!(files_fact(&whole).as_deref(), Some("4 shards"));
    assert_eq!(
        files_fact(&single),
        None,
        "one whole file is described by its size and its path"
    );
}

#[test]
fn a_listing_prints_as_one_civitai_fact() {
    assert_eq!(
        listing_fact(Some(&listed("One obsession", "v24"))),
        Some(Fact::new("civitai", "One obsession v24"))
    );
    assert_eq!(
        listing_fact(Some(&listed("One obsession", ""))),
        Some(Fact::new("civitai", "One obsession")),
        "a version the listing does not name leaves no trailing space"
    );
    assert_eq!(listing_fact(None), None);
}

#[test]
fn the_civitai_fact_sits_between_the_files_and_the_role_rows() {
    let row = SearchRow {
        role: Role::Lora,
        shard_count: 2,
        incomplete: false,
        shard_total: Some(2),
        ..search_row(RowFacts::default())
    };

    let keys: Vec<String> = facts(&row, Some(&listed("One obsession", "v24")))
        .into_iter()
        .map(|fact| fact.key)
        .collect();

    assert_eq!(
        keys,
        [
            "role",
            "base",
            "format",
            "size",
            "node",
            "layout",
            "blob",
            "files",
            "civitai",
            "rank / alpha"
        ],
        "what a listing named the bytes comes after the files and before the role rows"
    );
}
