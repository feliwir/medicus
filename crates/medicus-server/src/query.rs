//! Complete query answers: the index evaluates what it can, the stored headers the rest.
//!
//! The index reports constrained keys it cannot evaluate (attributes it does not index, sequence
//! matching) as `unfiltered`, and it only returns the attributes it stores. Both gaps are closed
//! here from the header of one stored instance of each candidate row (a range read of the file's
//! `0..header_length`). Above the instance level that instance stands for the whole entity, which
//! is exact for attributes that are, as the standard requires, the same in all its instances.

use medicus_core::capability::index::{DicomJsonObject, QueryResult, ResourceUid};
use medicus_core::query::DicomJson;
use medicus_core::query::{Attributes, Key, Matcher, Query};
use medicus_core::uid::SopInstanceUid;
use medicus_core::{Error, QueryLevel, Result, Tag, TagExt as _, tags};

use crate::archive::Archive;
use crate::ingest::parse;

/// Index rows fetched per round trip while post-filtering.
const PAGE: u32 = 500;

impl Archive {
    /// Runs `query` to a complete answer (see the module documentation).
    pub(crate) async fn query(&self, query: &Query) -> Result<QueryResult> {
        let fill: Vec<Tag> = query
            .return_tags()
            .filter(|t| !self.index_provides(*t, query.level))
            .collect();
        let first = self.index.query(query).await?;
        if first.unfiltered.is_empty() && fill.is_empty() {
            return Ok(first);
        }
        let post: Vec<&Key> = query
            .keys
            .iter()
            .filter(|k| first.unfiltered.contains(&k.tag))
            .collect();
        if post.is_empty() {
            // Only attributes to add: the index's paging is exact.
            let mut result = first;
            for row in &mut result.rows {
                self.complete(query, row, &post, &fill).await?;
            }
            return Ok(result);
        }

        // Post-filtering: page through all candidates, apply offset and limit to the matches.
        let wanted = query.limit.map(|l| query.offset + u64::from(l));
        let mut candidates = query.clone();
        candidates.limit = Some(PAGE);
        candidates.offset = 0;
        let mut matches = Vec::new();
        let mut more = true;
        while more && wanted.is_none_or(|w| (matches.len() as u64) <= w) {
            let page = self.index.query(&candidates).await?;
            more = page.has_more;
            candidates.offset += u64::from(PAGE);
            for mut row in page.rows {
                if self.complete(query, &mut row, &post, &fill).await? {
                    matches.push(row);
                }
            }
        }
        let skip = usize::try_from(query.offset).unwrap_or(usize::MAX);
        let mut rows: Vec<DicomJsonObject> = matches.into_iter().skip(skip).collect();
        let has_more = match query.limit {
            Some(limit) => {
                let limit = limit as usize;
                let more_rows = rows.len() > limit || more;
                rows.truncate(limit);
                more_rows
            }
            None => false,
        };
        Ok(QueryResult {
            rows,
            has_more,
            unfiltered: Vec::new(),
        })
    }

    /// Whether the index stores (or computes) `tag` for rows of `level`.
    fn index_provides(&self, tag: Tag, level: QueryLevel) -> bool {
        tags::standard(tag).is_some_and(|a| a.level <= level)
            || self
                .extra_tags
                .iter()
                .any(|e| e.tag == tag && e.level <= level)
    }

    /// Matches `row` against the `post` keys and adds the `fill` attributes, both from the header
    /// of one of its instances. Returns whether the row matches.
    async fn complete(
        &self,
        query: &Query,
        row: &mut DicomJsonObject,
        post: &[&Key],
        fill: &[Tag],
    ) -> Result<bool> {
        let Some(instance) = self.representative(query.level, row).await? else {
            // No stored instance (e.g. deleted meanwhile): only universal keys can match.
            return Ok(post.iter().all(|k| matches!(k.matcher, Matcher::Universal)));
        };
        let header = self.header(&instance).await?;
        let wanted = post.iter().map(|k| k.tag).chain(fill.iter().copied());
        let attributes = parse::to_json(&header, wanted)?;
        let entity = DicomJson::new(&attributes);
        if !post.iter().all(|k| k.matches(&entity, &query.options)) {
            return Ok(false);
        }
        for tag in fill {
            let key = tag.json_key();
            if !row.contains_key(&key)
                && let Some(value) = attributes.get(&key)
            {
                row.insert(key, value.clone());
            }
        }
        Ok(true)
    }

    /// One stored instance of the entity a result row describes.
    async fn representative(
        &self,
        level: QueryLevel,
        row: &DicomJsonObject,
    ) -> Result<Option<SopInstanceUid>> {
        let uid_of = |tag: Tag| {
            first(row, tag).ok_or_else(|| Error::internal(format!("query row lacks {tag}")))
        };
        let (tag, value) = match level {
            QueryLevel::Instance => {
                return Ok(Some(
                    SopInstanceUid::parse(&uid_of(tags::SOP_INSTANCE_UID)?)
                        .map_err(|e| Error::internal(e.to_string()))?,
                ));
            }
            QueryLevel::Series => (
                tags::SERIES_INSTANCE_UID,
                uid_of(tags::SERIES_INSTANCE_UID)?,
            ),
            QueryLevel::Study => (tags::STUDY_INSTANCE_UID, uid_of(tags::STUDY_INSTANCE_UID)?),
            QueryLevel::Patient => (tags::PATIENT_ID, uid_of(tags::PATIENT_ID)?),
        };
        let vr = tags::standard_vr(tag).unwrap_or(medicus_core::VR::UI);
        let mut one = Query::new(QueryLevel::Instance)
            .with_key(Key {
                tag,
                vr,
                matcher: Matcher::Single { value },
            })
            .with_key(Key::universal(tags::SOP_INSTANCE_UID, medicus_core::VR::UI));
        one.limit = Some(1);
        let rows = self.index.query(&one).await?.rows;
        rows.first()
            .and_then(|r| first(r, tags::SOP_INSTANCE_UID))
            .map(|uid| SopInstanceUid::parse(&uid).map_err(|e| Error::internal(e.to_string())))
            .transpose()
    }

    /// The parsed stored header of `instance`.
    async fn header(&self, instance: &SopInstanceUid) -> Result<dicom_object::DefaultDicomObject> {
        let location = self
            .index
            .locate(&ResourceUid::Instance(instance.clone()))
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| Error::not_found(format!("instance {instance} does not exist")))?;
        let stream = self
            .store(&location.blob.store)?
            .get(&location.blob.key, Some(0..location.header_length))
            .await?;
        let bytes = read_all(stream).await?;
        tokio::task::spawn_blocking(move || parse::parse_header(&bytes))
            .await
            .map_err(|e| Error::internal(format!("header parser failed: {e}")))?
    }
}

fn first(row: &DicomJsonObject, tag: Tag) -> Option<String> {
    Attributes::values(row, tag)?
        .into_iter()
        .next()
        .map(std::borrow::Cow::into_owned)
}

async fn read_all(stream: medicus_core::capability::ByteStream) -> Result<Vec<u8>> {
    use futures::TryStreamExt as _;
    let parts: Vec<bytes::Bytes> = stream.try_collect().await?;
    Ok(parts.concat())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use medicus_core::VR;
    use medicus_core::event::Origin;
    use medicus_core::query::MatchOptions;
    use medicus_testkit::dicom::InstanceBuilder;
    use medicus_testkit::stream;

    async fn archive(dir: &std::path::Path) -> Archive {
        let mut cfg = Config::default();
        cfg.server.data_dir = dir.to_owned();
        let caps = crate::addons::load(&cfg, &tokio::runtime::Handle::current()).unwrap();
        Archive::new(&caps, &cfg.archive).await.unwrap()
    }

    fn uids(result: &QueryResult, tag: Tag) -> Vec<String> {
        result.rows.iter().filter_map(|r| first(r, tag)).collect()
    }

    /// Five studies; `ManufacturerModelName` (not indexed) is "A" for studies 0, 2 and 4.
    async fn populated(dir: &std::path::Path) -> Archive {
        let archive = archive(dir).await;
        for i in 0..5 {
            let institution = if i % 2 == 0 { "A" } else { "B" };
            let file = InstanceBuilder::new(
                &format!("1.2.{i}"),
                &format!("1.2.{i}.1"),
                &format!("1.2.{i}.1.1"),
            )
            .attr(tags::MANUFACTURER_MODEL_NAME, VR::LO, institution)
            .attr(tags::STUDY_DATE, VR::DA, &format!("2024010{i}"))
            .build();
            let origin = Origin::Internal {
                component: "test".into(),
            };
            archive.ingest(stream(&file, 4096), origin).await.unwrap();
        }
        archive
    }

    fn study_query() -> Query {
        let mut q = Query::new(QueryLevel::Study)
            .with_key(Key::universal(tags::STUDY_INSTANCE_UID, VR::UI))
            .with_key(
                Key::parse(
                    tags::MANUFACTURER_MODEL_NAME,
                    VR::LO,
                    "A",
                    &MatchOptions::default(),
                )
                .unwrap(),
            );
        q.order_by.push(medicus_core::query::OrderBy {
            key: medicus_core::query::OrderKey::Attribute(tags::STUDY_DATE),
            descending: false,
        });
        q
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn filters_on_attributes_the_index_does_not_store() {
        let dir = tempfile::tempdir().unwrap();
        let archive = populated(dir.path()).await;
        let result = archive.query(&study_query()).await.unwrap();
        assert!(result.unfiltered.is_empty());
        assert!(!result.has_more);
        assert_eq!(
            uids(&result, tags::STUDY_INSTANCE_UID),
            ["1.2.0", "1.2.2", "1.2.4"]
        );
        // The matched attribute is returned too, although the index does not store it.
        assert_eq!(
            uids(&result, tags::MANUFACTURER_MODEL_NAME),
            ["A", "A", "A"]
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn pages_over_post_filtered_rows() {
        let dir = tempfile::tempdir().unwrap();
        let archive = populated(dir.path()).await;
        let mut q = study_query();
        q.limit = Some(1);
        q.offset = 1;
        let result = archive.query(&q).await.unwrap();
        assert_eq!(uids(&result, tags::STUDY_INSTANCE_UID), ["1.2.2"]);
        assert!(result.has_more);
        q.offset = 2;
        let result = archive.query(&q).await.unwrap();
        assert_eq!(uids(&result, tags::STUDY_INSTANCE_UID), ["1.2.4"]);
        assert!(!result.has_more);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn fills_return_attributes_from_the_header() {
        let dir = tempfile::tempdir().unwrap();
        let archive = populated(dir.path()).await;
        let mut q = Query::new(QueryLevel::Series)
            .with_key(
                Key::parse(
                    tags::STUDY_INSTANCE_UID,
                    VR::UI,
                    "1.2.1",
                    &MatchOptions::default(),
                )
                .unwrap(),
            )
            .with_key(Key::universal(tags::MANUFACTURER_MODEL_NAME, VR::LO));
        q.limit = Some(10);
        let result = archive.query(&q).await.unwrap();
        assert_eq!(uids(&result, tags::SERIES_INSTANCE_UID), ["1.2.1.1"]);
        assert_eq!(uids(&result, tags::MANUFACTURER_MODEL_NAME), ["B"]);
    }
}
