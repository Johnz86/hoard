use tantivy::Index;
use tantivy::tokenizer::{
    LowerCaser, RemoveLongFilter, TextAnalyzer, Token, TokenStream, Tokenizer,
};

/// The name the `text` field's schema gives the analyzer below, and the name every index the
/// catalog creates or opens registers it under.
pub(in crate::catalog) const WORD_TOKENIZER: &str = "hoard_words";
/// A word of this many bytes or more is dropped, as the `default` analyzer drops it.
pub(in crate::catalog) const LONGEST_WORD: usize = 40;

/// The analyzer the `text` field is indexed with and every free-text query is cut by.
pub(in crate::catalog) fn text_analyzer() -> TextAnalyzer {
    TextAnalyzer::builder(WordTokenizer::default())
        .filter(RemoveLongFilter::limit(LONGEST_WORD))
        .filter(LowerCaser)
        .build()
}

/// Registers that analyzer on `index` under [`WORD_TOKENIZER`]. Every index the catalog
/// creates or opens calls this before it makes a writer, because a segment writer keeps the
/// tokenizers the index held when the writer was made.
pub(in crate::catalog) fn register(index: &Index) {
    index.tokenizers().register(WORD_TOKENIZER, text_analyzer());
}

/// Cuts text into words at every character that is neither a letter nor a digit, at every
/// step between a letter and a digit, and before an uppercase letter that follows a
/// lowercase one unless another uppercase letter follows it, so `cyberrealisticPony_v160`
/// is `cyberrealistic`, `Pony`, `v`, `160`, a run of capitals stays one word and `LoRA`
/// stays one word.
#[derive(Clone, Default)]
struct WordTokenizer {
    token: Token,
}

struct WordStream<'a> {
    text: &'a str,
    cursor: usize,
    token: &'a mut Token,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Class {
    Lower,
    Upper,
    Digit,
    Other,
}

impl Class {
    fn of(character: char) -> Self {
        if character.is_numeric() {
            return Self::Digit;
        }
        if !character.is_alphabetic() {
            return Self::Other;
        }
        match character.is_uppercase() {
            true => Self::Upper,
            false => Self::Lower,
        }
    }

    fn breaks_before(self, next: Self, after: Option<Self>) -> bool {
        match (self, next) {
            (Class::Lower, Class::Upper) => after != Some(Class::Upper),
            (Class::Lower, Class::Lower)
            | (Class::Upper, Class::Lower)
            | (Class::Upper, Class::Upper)
            | (Class::Digit, Class::Digit) => false,
            (Class::Lower, Class::Digit)
            | (Class::Lower, Class::Other)
            | (Class::Upper, Class::Digit)
            | (Class::Upper, Class::Other)
            | (Class::Digit, Class::Lower)
            | (Class::Digit, Class::Upper)
            | (Class::Digit, Class::Other)
            | (Class::Other, Class::Lower)
            | (Class::Other, Class::Upper)
            | (Class::Other, Class::Digit)
            | (Class::Other, Class::Other) => true,
        }
    }
}

impl Tokenizer for WordTokenizer {
    type TokenStream<'a> = WordStream<'a>;

    fn token_stream<'a>(&'a mut self, text: &'a str) -> WordStream<'a> {
        self.token.reset();
        WordStream {
            text,
            cursor: 0,
            token: &mut self.token,
        }
    }
}

impl TokenStream for WordStream<'_> {
    fn advance(&mut self) -> bool {
        self.token.text.clear();
        self.token.position = self.token.position.wrapping_add(1);
        let rest = &self.text[self.cursor..];
        let mut start = None;
        let mut end = rest.len();
        let mut previous = Class::Other;
        let mut characters = rest.char_indices().peekable();
        while let Some((offset, character)) = characters.next() {
            let class = Class::of(character);
            let after = characters.peek().map(|&(_, next)| Class::of(next));
            match start {
                None => {
                    if class != Class::Other {
                        start = Some(offset);
                        previous = class;
                    }
                }
                Some(_) => {
                    if previous.breaks_before(class, after) {
                        end = offset;
                        break;
                    }
                    previous = class;
                }
            }
        }
        let Some(start) = start else {
            self.cursor = self.text.len();
            return false;
        };
        self.token.offset_from = self.cursor + start;
        self.token.offset_to = self.cursor + end;
        self.token
            .text
            .push_str(&self.text[self.token.offset_from..self.token.offset_to]);
        self.cursor += end;
        true
    }

    fn token(&self) -> &Token {
        self.token
    }

    fn token_mut(&mut self) -> &mut Token {
        self.token
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cut(text: &str) -> Vec<(usize, usize, usize, String)> {
        let mut analyzer = text_analyzer();
        let mut stream = analyzer.token_stream(text);
        let mut words = Vec::new();
        while let Some(token) = stream.next() {
            words.push((
                token.position,
                token.offset_from,
                token.offset_to,
                token.text.clone(),
            ));
        }
        words
    }

    fn words(text: &str) -> Vec<String> {
        cut(text).into_iter().map(|(_, _, _, word)| word).collect()
    }

    fn positions(text: &str) -> Vec<usize> {
        cut(text)
            .into_iter()
            .map(|(position, _, _, _)| position)
            .collect()
    }

    #[test]
    fn a_name_is_cut_at_punctuation_case_steps_and_digits() {
        let expected: [(&str, &[&str]); 13] = [
            (
                "cyberrealisticPony_v160.safetensors",
                &["cyberrealistic", "pony", "v", "160", "safetensors"],
            ),
            ("Qwen3-VL-8B", &["qwen", "3", "vl", "8", "b"]),
            ("Q4_K_M", &["q", "4", "k", "m"]),
            ("gpt-oss-120b", &["gpt", "oss", "120", "b"]),
            (
                "loxsSoulsMixPerfectDoll_v30",
                &["loxs", "souls", "mix", "perfect", "doll", "v", "30"],
            ),
            ("PONYTAIL", &["ponytail"]),
            ("NVFP4", &["nvfp", "4"]),
            ("oneObsession_v24", &["one", "obsession", "v", "24"]),
            ("A style LoRA.", &["a", "style", "lora"]),
            ("NoobAI-XL", &["noobai", "xl"]),
            ("ZImageTurbo", &["zimage", "turbo"]),
            ("styleV2", &["style", "v", "2"]),
            ("myLoRAFile", &["my", "lorafile"]),
        ];
        for (text, cut) in expected {
            assert_eq!(words(text), cut, "{text}");
            let counted: Vec<usize> = (0..cut.len()).collect();
            assert_eq!(positions(text), counted, "{text}");
        }
    }

    #[test]
    fn a_word_of_forty_bytes_or_more_is_dropped_and_keeps_its_position() {
        let overlong = "a".repeat(45);
        let text = format!("gpt-{overlong}-oss");
        assert_eq!(words(&text), ["gpt", "oss"]);
        assert_eq!(positions(&text), [0, 2]);

        let exact = "b".repeat(LONGEST_WORD);
        assert_eq!(words(&format!("gpt-{exact}-oss")), ["gpt", "oss"]);
        let kept = "c".repeat(LONGEST_WORD - 1);
        assert_eq!(
            words(&format!("gpt-{kept}-oss")),
            ["gpt", kept.as_str(), "oss"]
        );
    }

    #[test]
    fn offsets_cover_each_word_exactly() {
        let offsets: Vec<(usize, usize)> = cut("abcDef9x")
            .into_iter()
            .map(|(_, from, to, _)| (from, to))
            .collect();
        assert_eq!(offsets, [(0, 3), (3, 6), (6, 7), (7, 8)]);
    }
}
