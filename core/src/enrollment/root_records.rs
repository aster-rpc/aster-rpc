//! Shared exact-root Aster Docs read boundary for application policy records.
//!
//! Parsing bytes from elsewhere never creates this evidence. Consumers still
//! own their schemas, monotonic revisions, and deny-on-uncertainty decisions.

use crate::CoreDoc;
use iroh_docs::{store::Query, AuthorId};

pub fn validate_root_record_json(bytes: &[u8]) -> Result<(), RootRecordError> {
    super::jwt::validate_unique_json(bytes).map_err(|_| RootRecordError::InvalidJson)
}

#[derive(Debug)]
pub enum RootRecordError {
    WrongNamespace,
    TooManyRecords,
    OversizedRecord,
    InvalidJson,
    Storage(String),
}
impl std::fmt::Display for RootRecordError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "root-authored Docs record: {self:?}")
    }
}
impl std::error::Error for RootRecordError {}
fn storage(error: impl std::fmt::Display) -> RootRecordError {
    RootRecordError::Storage(error.to_string())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RootRecordRead {
    Absent,
    Pending,
    Available(Vec<u8>),
}

#[derive(Clone)]
pub struct RootAuthoredDocView {
    namespace: [u8; 32],
    root: [u8; 32],
    max_record_bytes: u64,
    max_records: usize,
}

impl RootAuthoredDocView {
    pub fn new(
        namespace: [u8; 32],
        root: [u8; 32],
        max_record_bytes: u64,
        max_records: usize,
    ) -> Self {
        Self {
            namespace,
            root,
            max_record_bytes,
            max_records,
        }
    }

    fn check_doc(&self, doc: &CoreDoc) -> Result<(), RootRecordError> {
        if doc.doc.id().as_bytes() == &self.namespace {
            Ok(())
        } else {
            Err(RootRecordError::WrongNamespace)
        }
    }

    pub async fn read_exact(
        &self,
        doc: &CoreDoc,
        key: Vec<u8>,
    ) -> Result<RootRecordRead, RootRecordError> {
        self.check_doc(doc)?;
        let entry = doc
            .doc
            .get_exact(AuthorId::from(self.root), key, false)
            .await
            .map_err(storage)?;
        let Some(entry) = entry else {
            return Ok(RootRecordRead::Absent);
        };
        self.read_entry(doc, &entry).await
    }

    async fn read_entry(
        &self,
        doc: &CoreDoc,
        entry: &iroh_docs::sync::Entry,
    ) -> Result<RootRecordRead, RootRecordError> {
        if entry.content_len() > self.max_record_bytes {
            return Err(RootRecordError::OversizedRecord);
        }
        let value = doc
            .read_entry_contents_if_complete(vec![entry.content_hash().to_hex().to_string()])
            .await
            .map_err(storage)?
            .pop()
            .flatten();
        Ok(match value {
            Some(bytes) => RootRecordRead::Available(bytes),
            None => RootRecordRead::Pending,
        })
    }

    /// Query only exact-root entries under this application's fixed prefix.
    /// Other authors and unrelated prefixes cannot hide a later denial.
    pub async fn scan_prefix(
        &self,
        doc: &CoreDoc,
        prefix: &[u8],
    ) -> Result<Vec<(Vec<u8>, Result<RootRecordRead, RootRecordError>)>, RootRecordError> {
        self.check_doc(doc)?;
        let entries = doc
            .doc
            .get_many_vec(
                Query::author(AuthorId::from(self.root))
                    .key_prefix(prefix)
                    .limit((self.max_records + 1) as u64)
                    .build(),
            )
            .await
            .map_err(storage)?;
        if entries.len() > self.max_records {
            return Err(RootRecordError::TooManyRecords);
        }
        let mut records = Vec::with_capacity(entries.len());
        for entry in entries {
            let key = entry.key().to_vec();
            let value = self.read_entry(doc, &entry).await;
            records.push((key, value));
        }
        Ok(records)
    }
}
