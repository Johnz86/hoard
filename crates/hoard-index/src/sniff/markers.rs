//! The rule vocabulary a header carries, answered over every tensor name it holds.

use std::collections::BTreeSet;
use std::sync::OnceLock;

use aho_corasick::AhoCorasick;
use hoard_core::{Markers, TensorSample};

use crate::infer::rules::vocabulary;

/// What a checkpoint puts in front of the keys a family is named by, longest first.
const WRAPPER_PREFIXES: [&str; 4] = [
    "model.diffusion_model.",
    "diffusion_model.",
    "model.",
    "net.",
];

/// Every vocabulary term found among these tensor names.
pub(crate) fn markers_of(tensors: &[TensorSample]) -> Markers {
    let vocabulary = vocabulary();
    let fragments = match fragment_automaton() {
        Some(automaton) => scanned_fragments(automaton, &vocabulary.fragments, tensors),
        None => matched_terms(&vocabulary.fragments, tensors, |name, term| {
            name.contains(term)
        }),
    };
    Markers {
        fragments,
        prefixes: matched_terms(&vocabulary.prefixes, tensors, |name, term| {
            name.starts_with(term)
        }),
        keys: matched_keys(&vocabulary.keys, tensors),
    }
}

fn fragment_automaton() -> Option<&'static AhoCorasick> {
    static AUTOMATON: OnceLock<Option<AhoCorasick>> = OnceLock::new();
    AUTOMATON
        .get_or_init(|| match AhoCorasick::new(&vocabulary().fragments) {
            Ok(automaton) => Some(automaton),
            Err(error) => {
                tracing::error!(%error, "the rule vocabulary does not build into an automaton");
                None
            }
        })
        .as_ref()
}

fn scanned_fragments(
    automaton: &AhoCorasick,
    terms: &[&'static str],
    tensors: &[TensorSample],
) -> BTreeSet<String> {
    let mut found = vec![false; terms.len()];
    let mut remaining = terms.len();
    for tensor in tensors {
        if remaining == 0 {
            break;
        }
        for hit in automaton.find_overlapping_iter(&tensor.name) {
            let slot = &mut found[hit.pattern().as_usize()];
            if !*slot {
                *slot = true;
                remaining -= 1;
            }
        }
    }
    terms
        .iter()
        .zip(found)
        .filter(|(_, hit)| *hit)
        .map(|(term, _)| (*term).to_owned())
        .collect()
}

fn matched_terms(
    terms: &[&'static str],
    tensors: &[TensorSample],
    hit: fn(&str, &str) -> bool,
) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    for tensor in tensors {
        if found.len() == terms.len() {
            break;
        }
        for term in terms {
            if !found.contains(*term) && hit(&tensor.name, term) {
                found.insert((*term).to_owned());
            }
        }
    }
    found
}

fn matched_keys(keys: &[&'static str], tensors: &[TensorSample]) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    for tensor in tensors {
        let name = tensor.name.as_str();
        let candidates = std::iter::once(name).chain(
            WRAPPER_PREFIXES
                .iter()
                .filter_map(|wrapper| name.strip_prefix(wrapper)),
        );
        for candidate in candidates {
            if let Ok(index) = keys.binary_search_by(|known| (*known).cmp(candidate)) {
                found.insert(keys[index].to_owned());
            }
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn named(names: &[&str]) -> Vec<TensorSample> {
        names
            .iter()
            .map(|name| TensorSample {
                name: (*name).to_owned(),
                dtype: "F16".to_owned(),
                shape: vec![4, 4],
            })
            .collect()
    }

    #[test]
    fn a_key_matches_under_each_wrapper_prefix_and_not_inside_a_longer_name() {
        let table = ["label_emb.0.0.weight"];
        for wrapper in [
            "",
            "model.diffusion_model.",
            "diffusion_model.",
            "model.",
            "net.",
        ] {
            let name = format!("{wrapper}label_emb.0.0.weight");
            assert!(
                matched_keys(&table, &named(&[&name])).contains("label_emb.0.0.weight"),
                "{name} names the key"
            );
        }
        assert!(
            matched_keys(&table, &named(&["extra.label_emb.0.0.weight"])).is_empty(),
            "a key is the whole name, not a suffix of one"
        );
        assert!(
            markers_of(&named(&["extra.label_emb.0.0.weight"]))
                .fragments
                .contains("label_emb"),
            "the fragment is still found inside the longer name"
        );
    }

    #[test]
    fn a_fragment_and_a_prefix_are_read_from_the_same_names() {
        let markers = markers_of(&named(&[
            "lora_unet_down_blocks_0_attn.lora_down.weight",
            "first_stage_model.decoder.conv_in.weight",
        ]));
        assert!(markers.prefixes.contains("lora_unet_"));
        assert!(markers.fragments.contains(".lora_down"));
        assert!(markers.prefixes.contains("first_stage_model."));
        assert!(!markers.prefixes.contains("conditioner."));
    }

    #[test]
    fn the_linear_fallback_finds_what_the_automaton_finds() {
        let names = named(&[
            "model.diffusion_model.input_blocks.0.0.weight",
            "conditioner.embedders.0.model.token_embedding.weight",
            "first_stage_model.decoder.conv_in.weight",
        ]);
        let terms = &vocabulary().fragments;
        let automaton = fragment_automaton().expect("the vocabulary builds an automaton");
        assert_eq!(
            scanned_fragments(automaton, terms, &names),
            matched_terms(terms, &names, |name, term| name.contains(term))
        );
    }
}
