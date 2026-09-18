use std::path::Path;
use std::sync::OnceLock;

use native_db::transaction::RTransaction;
use native_db::{Database, Models, ToKey, native_db};
use native_model::{Model, native_model};
use serde::{Deserialize, Serialize};

use hoard_core::Sha256;

use crate::error::{IndexError, Result};
use crate::time::{now, timestamp_text};

use super::Catalog;
use super::hash_memo::{memo_builder, stamped_version};

const LOOKUP_MEMO_VERSION: u32 = 2;

const LOOKUP_MEMO_DB_FILE: &str = "civitai.redb";

const LOOKUP_MEMO_STAMP_FILE: &str = "civitai.version";

/// What a listing service said about one artifact's exact bytes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Listed {
    pub base_model: String,
    pub model: String,
    pub version: String,
    pub kind: String,
    /// The words the uploader says trigger the model, in the order listed, possibly empty.
    pub trained_words: Vec<String>,
}

/// One question already asked about one sha256, and the answer it got. `found` is `None` when
/// the service was asked and knows nothing about those bytes, which is an answer too and is
/// remembered so it is never asked again on the next pass.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LookupAnswer {
    pub asked_at: String,
    pub found: Option<Listed>,
}

impl LookupAnswer {
    /// An answer as a pass has just received it, stamped the way every other instant this
    /// catalog stores is, so two of them compare as text the way they compare as time.
    pub fn asked_now(found: Option<Listed>) -> Self {
        Self {
            asked_at: timestamp_text(now()),
            found,
        }
    }
}

/// Every answer a lookup pass has ever received, against the sha256 it asked about.
///
/// It lives beside the catalog directory rather than inside it, exactly as the digest memo
/// does, so a catalog rebuilt after a `FORMAT_VERSION` change asks a third party nothing
/// again. It carries its own version stamp: a memo written in another shape, or one whose
/// file is corrupt, is deleted and started again empty rather than converted. A file another
/// handle holds the lock on is reported rather than destroyed.
pub(super) struct LookupMemo {
    database: Database<'static>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[native_model(id = 1, version = 1, with = super::codec::Compact)]
#[native_db]
struct Answered {
    #[primary_key]
    sha256: Vec<u8>,
    asked_at: String,
    found: Option<Listed>,
}

impl LookupMemo {
    pub(super) fn open(data_dir: &Path) -> Result<Self> {
        let database = data_dir.join(LOOKUP_MEMO_DB_FILE);
        let stamp = data_dir.join(LOOKUP_MEMO_STAMP_FILE);
        if stamped_version(&stamp) == Some(LOOKUP_MEMO_VERSION) {
            match memo_builder().open(lookup_models()?, &database) {
                Ok(opened) => return Ok(Self { database: opened }),
                Err(error) => match IndexError::from(error) {
                    held @ IndexError::CatalogBusy => return Err(held),
                    unreadable => tracing::debug!(
                        path = %database.display(),
                        error = %unreadable,
                        "the lookup memo does not open; starting a fresh one"
                    ),
                },
            }
        }
        std::fs::create_dir_all(data_dir).map_err(|error| IndexError::io(data_dir, error))?;
        let _ = std::fs::remove_file(&database);
        let _ = std::fs::remove_file(&stamp);
        let created = memo_builder().create(lookup_models()?, &database)?;
        std::fs::write(&stamp, format!("{LOOKUP_MEMO_VERSION}\n"))
            .map_err(|error| IndexError::io(&stamp, error))?;
        Ok(Self { database: created })
    }

    pub(super) fn answer(&self, sha256: &Sha256) -> Result<Option<LookupAnswer>> {
        self.reading()?.answer(sha256)
    }

    /// One read snapshot of the memo, for a caller asking about many hashes in a row: a
    /// listing then opens one transaction rather than one per artifact it considers.
    pub(super) fn reading(&self) -> Result<Answers<'_>> {
        Ok(Answers {
            read: self.database.r_transaction()?,
        })
    }

    /// Writes a whole batch in one transaction, because redb fsyncs every commit.
    pub(super) fn remember(&self, answers: &[(Sha256, LookupAnswer)]) -> Result<()> {
        if answers.is_empty() {
            return Ok(());
        }
        let transaction = self.database.rw_transaction()?;
        for (sha256, answer) in answers {
            transaction.upsert(Answered {
                sha256: sha256.as_bytes().to_vec(),
                asked_at: answer.asked_at.clone(),
                found: answer.found.clone(),
            })?;
        }
        transaction.commit()?;
        Ok(())
    }
}

/// What the memo held when the snapshot was taken.
pub(super) struct Answers<'memo> {
    read: RTransaction<'memo>,
}

impl Answers<'_> {
    pub(super) fn answer(&self, sha256: &Sha256) -> Result<Option<LookupAnswer>> {
        let stored: Option<Answered> = self.read.get().primary(sha256.as_bytes().to_vec())?;
        Ok(stored.map(|row| LookupAnswer {
            asked_at: row.asked_at,
            found: row.found,
        }))
    }
}

impl Catalog {
    /// What a lookup already learned about these bytes, or nothing when they were never
    /// asked about.
    pub fn lookup_answer(&self, sha256: &Sha256) -> Result<Option<LookupAnswer>> {
        self.lookup_memo()?.answer(sha256)
    }

    /// Remembers every answer of one batch, so no hash is ever asked about twice.
    pub fn remember_lookups(&self, answers: &[(Sha256, LookupAnswer)]) -> Result<()> {
        self.lookup_memo()?.remember(answers)
    }

    /// The memo, opened on first use and held for the life of the catalog: redb takes one
    /// exclusive lock per file, so it is opened once rather than per question.
    pub(super) fn lookup_memo(&self) -> Result<std::sync::Arc<LookupMemo>> {
        let mut held = self.lookup_memo.lock();
        if let Some(memo) = held.as_ref() {
            return Ok(std::sync::Arc::clone(memo));
        }
        let opened = std::sync::Arc::new(LookupMemo::open(self.memo_directory())?);
        *held = Some(std::sync::Arc::clone(&opened));
        Ok(opened)
    }
}

fn lookup_models() -> Result<&'static Models> {
    static MODELS: OnceLock<Models> = OnceLock::new();
    if let Some(models) = MODELS.get() {
        return Ok(models);
    }
    let mut defined = Models::new();
    defined.define::<Answered>()?;
    let _ = MODELS.set(defined);
    MODELS
        .get()
        .ok_or_else(|| IndexError::Catalog("lookup memo model registration failed".into()))
}

#[cfg(test)]
mod tests {
    use crate::config::IndexConfig;

    use super::*;

    fn sha(byte: u8) -> Sha256 {
        Sha256::from_bytes([byte; 32])
    }

    fn listed(base_model: &str) -> Listed {
        Listed {
            base_model: base_model.to_owned(),
            model: "One obsession".to_owned(),
            version: "v24".to_owned(),
            kind: "Checkpoint".to_owned(),
            trained_words: Vec::new(),
        }
    }

    fn answer(base_model: &str) -> LookupAnswer {
        LookupAnswer {
            asked_at: "2026-09-17T09:00:00.000000000Z".to_owned(),
            found: Some(listed(base_model)),
        }
    }

    fn unknown() -> LookupAnswer {
        LookupAnswer {
            asked_at: "2026-09-17T09:00:00.000000000Z".to_owned(),
            found: None,
        }
    }

    #[test]
    fn an_answer_is_remembered_across_a_reopen() -> Result<()> {
        let directory = tempfile::tempdir()?;
        {
            let memo = LookupMemo::open(directory.path())?;
            memo.remember(&[(sha(0x11), answer("Illustrious"))])?;
        }

        let reopened = LookupMemo::open(directory.path())?;

        assert_eq!(
            reopened.answer(&sha(0x11))?,
            Some(answer("Illustrious")),
            "what one pass was told, the next pass reads"
        );
        assert_eq!(
            reopened.answer(&sha(0x12))?,
            None,
            "a hash nobody asked about answers nothing"
        );
        Ok(())
    }

    #[test]
    fn an_unknown_hash_is_remembered_as_asked() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let memo = LookupMemo::open(directory.path())?;

        memo.remember(&[(sha(0x21), unknown())])?;

        let recalled = memo.answer(&sha(0x21))?.expect("the question was asked");
        assert_eq!(recalled.found, None, "asked and unknown is not unasked");
        assert_eq!(recalled.asked_at, "2026-09-17T09:00:00.000000000Z");
        Ok(())
    }

    #[test]
    fn a_memo_of_another_version_is_started_again_empty() -> Result<()> {
        let directory = tempfile::tempdir()?;
        {
            let memo = LookupMemo::open(directory.path())?;
            memo.remember(&[(sha(0x31), answer("Pony"))])?;
        }
        std::fs::write(
            directory.path().join(LOOKUP_MEMO_STAMP_FILE),
            format!("{}\n", LOOKUP_MEMO_VERSION + 1),
        )?;

        let reopened = LookupMemo::open(directory.path())?;

        assert_eq!(
            reopened.answer(&sha(0x31))?,
            None,
            "a memo of another version is started again, not read"
        );
        assert_eq!(
            stamped_version(&directory.path().join(LOOKUP_MEMO_STAMP_FILE)),
            Some(LOOKUP_MEMO_VERSION),
            "the fresh memo carries this build's version"
        );
        reopened.remember(&[(sha(0x32), answer("NoobAI"))])?;
        assert_eq!(
            reopened.answer(&sha(0x32))?,
            Some(answer("NoobAI")),
            "the wiped memo is usable, not merely empty"
        );
        Ok(())
    }

    #[test]
    fn a_memo_of_the_previous_version_is_started_again() -> Result<()> {
        let directory = tempfile::tempdir()?;
        {
            let memo = LookupMemo::open(directory.path())?;
            memo.remember(&[(sha(0x41), answer("Pony"))])?;
        }
        std::fs::write(
            directory.path().join(LOOKUP_MEMO_STAMP_FILE),
            format!("{}\n", LOOKUP_MEMO_VERSION - 1),
        )?;

        let reopened = LookupMemo::open(directory.path())?;

        assert_eq!(
            reopened.answer(&sha(0x41))?,
            None,
            "an answer of the previous shape is asked again rather than converted"
        );
        assert_eq!(
            stamped_version(&directory.path().join(LOOKUP_MEMO_STAMP_FILE)),
            Some(LOOKUP_MEMO_VERSION)
        );
        assert!(
            directory.path().join(LOOKUP_MEMO_DB_FILE).exists(),
            "the fresh memo is on disk beside its stamp"
        );
        Ok(())
    }

    #[test]
    fn the_memo_survives_a_catalog_of_another_format() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let config = IndexConfig {
            data_dir: directory.path().join("data"),
            sources: Vec::new(),
            ..IndexConfig::default()
        };
        let catalog = Catalog::open_or_scan(&config, "workstation", |_, _| {})?;
        catalog.remember_lookups(&[(sha(0x41), answer("Illustrious"))])?;
        drop(catalog);
        std::fs::remove_dir_all(config.native_path())?;

        let rebuilt = Catalog::open_or_scan(&config, "workstation", |_, _| {})?;

        assert_eq!(
            rebuilt.lookup_answer(&sha(0x41))?,
            Some(answer("Illustrious")),
            "a catalog rebuilt from disk asks a third party nothing again"
        );
        assert!(
            config.data_dir.join(LOOKUP_MEMO_DB_FILE).exists(),
            "the memo sits beside the catalog directory, not inside it"
        );
        Ok(())
    }

    #[test]
    fn a_corrupt_memo_is_replaced_rather_than_propagated() -> Result<()> {
        let directory = tempfile::tempdir()?;
        {
            let memo = LookupMemo::open(directory.path())?;
            memo.remember(&[(sha(0x51), answer("Pony"))])?;
        }
        std::fs::write(
            directory.path().join(LOOKUP_MEMO_DB_FILE),
            vec![0xff_u8; 8 * 1024],
        )?;

        let reopened = LookupMemo::open(directory.path())?;

        assert_eq!(reopened.answer(&sha(0x51))?, None);
        assert_eq!(
            reopened.remember(&[])?,
            (),
            "an empty batch opens no transaction and is not an error"
        );
        Ok(())
    }
}
