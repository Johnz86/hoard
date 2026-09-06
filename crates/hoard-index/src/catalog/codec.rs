use bincode::Options;
use serde::{Deserialize, Serialize};

use native_model::{Decode, Encode};

use crate::error::IndexError;

/// How the catalog's bulk records are written: bincode 1.3 with variable length integers,
/// little endian, and trailing bytes refused.
///
/// native_model's own default is bincode's fixed width form, which spends eight bytes on
/// every string and vector length, four on every enum discriminant and eight on every
/// integer whatever it holds. Nothing here is read by another program, so the compact form
/// costs nothing and is most of what a row of this catalog used to be.
fn options() -> impl Options {
    bincode::DefaultOptions::new()
        .with_little_endian()
        .with_varint_encoding()
        .reject_trailing_bytes()
}

/// The compact codec, named by `#[native_model(with = ...)]` on the records that carry the
/// catalog's bulk. native_model's eight byte header is written around it either way, so a
/// record in this codec is still identified before it is decoded.
#[derive(Default)]
pub(super) struct Compact;

impl<T: Serialize> Encode<T> for Compact {
    type Error = bincode::Error;

    fn encode(value: &T) -> Result<Vec<u8>, Self::Error> {
        options().serialize(value)
    }
}

impl<T: for<'de> Deserialize<'de>> Decode<T> for Compact {
    type Error = bincode::Error;

    fn decode(data: Vec<u8>) -> Result<T, Self::Error> {
        options().deserialize(&data)
    }
}

/// The largest payload a cold record may decode to.
///
/// The biggest metadata hoard accepts from a file is a 256 MiB GGUF header, and a cold
/// record holds a summary of one, never more than one. A length claiming more than this is
/// a corrupt block, and it is refused before anything is allocated for it.
const COLD_MAX: usize = 256 * 1024 * 1024;

/// The envelope's first byte: the compact bytes follow as they are.
const RAW: u8 = 0;
/// The envelope's first byte: a four byte decoded length follows, then an LZ4 block.
const LZ4: u8 = 1;

/// How much longer the compressed envelope is than the raw one, so a block is only kept
/// when it pays for its own tag and length.
const LZ4_OVERHEAD: usize = size_of::<u32>();

/// How the catalog's cold records are written: [`Compact`]'s bytes behind a tag, and an
/// LZ4 block instead when the whole envelope comes out smaller.
///
/// `ArtifactDetail` and `SearchText` are read one row at a time by the inspector and once
/// per artifact by a search rebuild, never scanned as a group, and they are the two largest
/// tables. Each row is compressed on its own, so reading one is still one primary lookup
/// and one bounded decode; nothing here is a block store or a second join.
#[derive(Default)]
pub(super) struct Cold;

impl<T: Serialize> Encode<T> for Cold {
    type Error = IndexError;

    fn encode(value: &T) -> Result<Vec<u8>, Self::Error> {
        let raw = options()
            .serialize(value)
            .map_err(|error| IndexError::Catalog(format!("cold record: {error}")))?;
        let length = u32::try_from(raw.len())
            .ok()
            .filter(|_| raw.len() <= COLD_MAX)
            .ok_or_else(|| {
                IndexError::Catalog(format!(
                    "cold record of {} bytes is beyond the {COLD_MAX} byte maximum",
                    raw.len()
                ))
            })?;
        let block = lz4_flex::block::compress(&raw);
        if block.len() + LZ4_OVERHEAD >= raw.len() {
            let mut envelope = Vec::with_capacity(1 + raw.len());
            envelope.push(RAW);
            envelope.extend_from_slice(&raw);
            return Ok(envelope);
        }
        let mut envelope = Vec::with_capacity(1 + LZ4_OVERHEAD + block.len());
        envelope.push(LZ4);
        envelope.extend_from_slice(&length.to_le_bytes());
        envelope.extend_from_slice(&block);
        Ok(envelope)
    }
}

impl<T: for<'de> Deserialize<'de>> Decode<T> for Cold {
    type Error = IndexError;

    fn decode(data: Vec<u8>) -> Result<T, Self::Error> {
        let (tag, body) = data
            .split_first()
            .ok_or_else(|| IndexError::Catalog("cold record is empty".into()))?;
        match *tag {
            RAW => decode_compact(body),
            LZ4 => {
                let (length, block) = body.split_at_checked(LZ4_OVERHEAD).ok_or_else(|| {
                    IndexError::Catalog("cold record has no decoded length".into())
                })?;
                let decoded = length
                    .try_into()
                    .map(u32::from_le_bytes)
                    .map(|length| length as usize)
                    .map_err(|error| IndexError::Catalog(format!("cold record: {error}")))?;
                if decoded > COLD_MAX {
                    return Err(IndexError::Catalog(format!(
                        "cold record claims {decoded} bytes, beyond the {COLD_MAX} byte maximum"
                    )));
                }
                let raw = lz4_flex::block::decompress(block, decoded).map_err(|error| {
                    IndexError::Catalog(format!("cold record does not decompress: {error}"))
                })?;
                if raw.len() != decoded {
                    return Err(IndexError::Catalog(format!(
                        "cold record decompressed to {} bytes, not the {decoded} it claims",
                        raw.len()
                    )));
                }
                decode_compact(&raw)
            }
            other => Err(IndexError::Catalog(format!(
                "cold record carries encoding {other}, which this build does not write"
            ))),
        }
    }
}

fn decode_compact<T: for<'de> Deserialize<'de>>(body: &[u8]) -> Result<T, IndexError> {
    options()
        .deserialize(body)
        .map_err(|error| IndexError::Catalog(format!("cold record: {error}")))
}

#[cfg(test)]
mod tests {
    use hoard_core::{Sha256, State};

    use super::*;

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Row {
        id: i64,
        name: String,
        flag: Option<u32>,
    }

    fn row() -> Row {
        Row {
            id: 7,
            name: "a".to_owned(),
            flag: Some(3),
        }
    }

    #[test]
    fn the_compact_codec_is_shorter_than_the_default_and_round_trips() {
        let compact = <Compact as Encode<Row>>::encode(&row()).expect("encodes");
        let default = native_model::bincode_1_3::Bincode::encode(&row()).expect("encodes");
        assert!(
            compact.len() < default.len(),
            "compact {} against default {}",
            compact.len(),
            default.len()
        );
        assert_eq!(compact.len(), 1 + 1 + 1 + 1 + 1, "one byte per small value");
        let decoded: Row = <Compact as Decode<Row>>::decode(compact).expect("decodes");
        assert_eq!(decoded, row());
    }

    #[test]
    fn compact_record_decoding_rejects_invalid_payloads() {
        let mut trailing = <Compact as Encode<Row>>::encode(&row()).expect("encodes");
        trailing.push(0);
        assert!(
            <Compact as Decode<Row>>::decode(trailing.clone()).is_err(),
            "a payload with something after it is refused"
        );
        let mut truncated = trailing;
        truncated.truncate(2);
        assert!(
            <Compact as Decode<Row>>::decode(truncated).is_err(),
            "a payload that stops early is refused"
        );

        let state = <Compact as Encode<State>>::encode(&State::Local).expect("encodes");
        assert_eq!(state.len(), 1, "a unit variant is one byte");
        assert!(
            <Compact as Decode<State>>::decode(vec![250]).is_err(),
            "a discriminant no variant has is refused"
        );

        let hash = Sha256::from_bytes([7; 32]);
        let encoded = <Compact as Encode<Option<Sha256>>>::encode(&Some(hash)).expect("encodes");
        assert_eq!(
            encoded.len(),
            1 + 1 + 32,
            "a present hash is its tag, its length and its bytes"
        );
        assert_eq!(
            <Compact as Decode<Option<Sha256>>>::decode(encoded.clone()).expect("decodes"),
            Some(hash),
            "a hash survives the compact form exactly"
        );
        for length in [0_u8, 31, 33, 200] {
            let mut malformed = encoded.clone();
            malformed[1] = length;
            assert!(
                <Compact as Decode<Option<Sha256>>>::decode(malformed).is_err(),
                "a hash of {length} bytes is refused"
            );
        }
    }

    /// A body a real card gives: repeated prose LZ4 shortens a long way.
    fn compressible(size: usize) -> String {
        "the same sentence over and over, which is what a model card reads like. ".repeat(size)
    }

    /// A body nothing shortens: every byte differs from the last in a way LZ4 cannot
    /// reference.
    fn incompressible(size: usize) -> Vec<u8> {
        let mut state = 0x2545_f491_4f6c_dd1d_u64;
        (0..size)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                (state >> 24) as u8
            })
            .collect()
    }

    #[test]
    fn cold_record_compression_preserves_every_detail() {
        for value in [
            String::new(),
            "x".to_owned(),
            compressible(1),
            compressible(4096),
        ] {
            let envelope = <Cold as Encode<String>>::encode(&value).expect("encodes");
            let decoded: String = <Cold as Decode<String>>::decode(envelope).expect("decodes");
            assert_eq!(decoded, value, "text of {} bytes survives", value.len());
        }

        let long = compressible(4096);
        let envelope = <Cold as Encode<String>>::encode(&long).expect("encodes");
        assert_eq!(envelope[0], LZ4, "repeated prose is worth compressing");
        assert!(
            envelope.len() < long.len() / 4,
            "a compressible card is much smaller: {} against {}",
            envelope.len(),
            long.len()
        );

        let noise = incompressible(4096);
        let envelope = <Cold as Encode<Vec<u8>>>::encode(&noise).expect("encodes");
        assert_eq!(envelope[0], RAW, "nothing is spent compressing noise");
        assert_eq!(
            <Cold as Decode<Vec<u8>>>::decode(envelope).expect("decodes"),
            noise
        );

        let empty = <Cold as Encode<Vec<u8>>>::encode(&Vec::<u8>::new()).expect("encodes");
        assert_eq!(empty, vec![RAW, 0], "an empty value is a tag and a length");
    }

    #[test]
    fn cold_record_decoding_checks_lengths_and_tags() {
        let value = compressible(4096);
        let envelope = <Cold as Encode<String>>::encode(&value).expect("encodes");
        assert_eq!(envelope[0], LZ4);

        for tag in [2_u8, 7, 255] {
            let mut tagged = envelope.clone();
            tagged[0] = tag;
            assert!(
                <Cold as Decode<String>>::decode(tagged).is_err(),
                "encoding {tag} is refused"
            );
        }
        assert!(
            <Cold as Decode<String>>::decode(Vec::new()).is_err(),
            "an empty envelope is refused"
        );
        assert!(
            <Cold as Decode<String>>::decode(vec![LZ4, 1, 2]).is_err(),
            "an envelope with no decoded length is refused"
        );

        let mut oversized = envelope.clone();
        oversized[1..5].copy_from_slice(&(COLD_MAX as u32 + 1).to_le_bytes());
        assert!(
            <Cold as Decode<String>>::decode(oversized).is_err(),
            "a length beyond the maximum is refused before it is allocated"
        );

        let mut wrong_length = envelope.clone();
        wrong_length[1..5].copy_from_slice(&7_u32.to_le_bytes());
        assert!(
            <Cold as Decode<String>>::decode(wrong_length).is_err(),
            "a length the block does not decompress to is refused"
        );

        let mut truncated = envelope.clone();
        truncated.truncate(envelope.len() / 2);
        assert!(
            <Cold as Decode<String>>::decode(truncated).is_err(),
            "a block that stops early is refused"
        );

        let mut corrupt = envelope.clone();
        let last = corrupt.len() - 1;
        corrupt[last] ^= 0xff;
        corrupt[6] ^= 0xff;
        assert!(
            <Cold as Decode<String>>::decode(corrupt).is_err(),
            "a block whose bytes changed is refused"
        );

        let mut raw = <Cold as Encode<Vec<u8>>>::encode(&incompressible(64)).expect("encodes");
        raw.push(0);
        assert!(
            <Cold as Decode<Vec<u8>>>::decode(raw).is_err(),
            "a raw envelope with something after it is refused"
        );
    }

    #[test]
    fn the_compact_form_of_a_hash_is_bytes_and_the_readable_one_is_hex() {
        let hash = Sha256::from_bytes([0xab; 32]);
        let json = serde_json::to_string(&hash).expect("renders");
        assert_eq!(json, format!("\"{}\"", "ab".repeat(32)));
        let compact = <Compact as Encode<Sha256>>::encode(&hash).expect("encodes");
        assert_eq!(compact, [&[32_u8][..], &[0xab; 32][..]].concat());
    }
}
