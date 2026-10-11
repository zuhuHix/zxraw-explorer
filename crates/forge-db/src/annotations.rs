//! Ratings, color labels, pick flags, notes and tags. Stored per file row (never in the
//! media file itself), so they survive the file going offline and move with
//! [`Db::relink`].

use forge_core::query::{ColorLabel, PickState};
use rusqlite::{OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};

use crate::error::{DbError, Result};
use crate::files::FileId;
use crate::{Db, now_ms};

/// Maximum accepted tag length in Unicode characters.
pub const MAX_TAG_NAME_CHARS: usize = 256;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Annotation {
    pub rating: u8,
    pub label: Option<ColorLabel>,
    pub pick: PickState,
    pub notes: Option<String>,
    /// Sorted by name.
    pub tags: Vec<String>,
}

/// A change applied to many files at once. `None` fields are left untouched.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AnnotationChange {
    pub rating: Option<u8>,
    /// `Some(None)` clears the label.
    pub label: Option<Option<ColorLabel>>,
    pub pick: Option<PickState>,
    /// `Some(None)` clears the notes.
    pub notes: Option<Option<String>>,
    pub add_tags: Vec<String>,
    pub remove_tags: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TagCount {
    pub name: String,
    pub files: u64,
}

fn clean_tag(t: &str) -> Option<String> {
    let t = t.trim();
    (!t.is_empty()).then(|| t.to_string())
}

pub(crate) fn tag_id_tx(tx: &Transaction<'_>, name: &str, create: bool) -> Result<Option<i64>> {
    let lc = name.to_lowercase();
    if create {
        tx.execute(
            "INSERT INTO tags(name, name_lc) VALUES (?1, ?2) ON CONFLICT(name_lc) DO NOTHING",
            params![name, lc],
        )?;
    }
    Ok(tx
        .query_row("SELECT id FROM tags WHERE name_lc = ?1", [lc], |r| r.get(0))
        .optional()?)
}

impl Db {
    /// Apply `change` to every file in `ids` in one transaction. Returns the number of files.
    pub fn annotate(&self, ids: &[FileId], change: &AnnotationChange) -> Result<usize> {
        if let Some(r) = change.rating
            && r > 5
        {
            return Err(DbError::Invalid(format!("rating {r} is out of range 0-5")));
        }
        let add = validated_tags(&change.add_tags)?;
        let remove = validated_tags(&change.remove_tags)?;
        let touches_row = change.rating.is_some()
            || change.label.is_some()
            || change.pick.is_some()
            || change.notes.is_some();
        self.with_tx(|tx| {
            let now = now_ms();
            let add_ids: Vec<i64> = add
                .iter()
                .map(|t| tag_id_tx(tx, t, true).map(|o| o.expect("created")))
                .collect::<Result<_>>()?;
            let mut remove_ids = Vec::new();
            for t in &remove {
                if let Some(id) = tag_id_tx(tx, t, false)? {
                    remove_ids.push(id);
                }
            }
            let mut ensure = tx.prepare_cached(
                "INSERT INTO annotations(file_id, updated_at) VALUES (?1, ?2) ON CONFLICT(file_id) DO NOTHING",
            )?;
            let mut upd = tx.prepare_cached(
                "UPDATE annotations SET
                    rating = CASE WHEN ?2 THEN ?3 ELSE rating END,
                    label  = CASE WHEN ?4 THEN ?5 ELSE label END,
                    pick   = CASE WHEN ?6 THEN ?7 ELSE pick END,
                    notes  = CASE WHEN ?8 THEN ?9 ELSE notes END,
                    updated_at = ?10
                 WHERE file_id = ?1",
            )?;
            let mut tag_add =
                tx.prepare_cached("INSERT OR IGNORE INTO file_tags(file_id, tag_id) VALUES (?1, ?2)")?;
            let mut tag_rm = tx.prepare_cached("DELETE FROM file_tags WHERE file_id = ?1 AND tag_id = ?2")?;
            let label = change.label.map(|l| l.map(|c| c.as_str()));
            let notes = change.notes.as_ref().map(|n| n.as_deref().filter(|s| !s.trim().is_empty()));
            for &id in ids {
                if touches_row {
                    ensure.execute(params![id, now])?;
                    upd.execute(params![
                        id,
                        change.rating.is_some(),
                        change.rating.unwrap_or(0),
                        label.is_some(),
                        label.flatten(),
                        change.pick.is_some(),
                        change.pick.unwrap_or_default().as_i64(),
                        notes.is_some(),
                        notes.flatten(),
                        now
                    ])?;
                }
                for t in &add_ids {
                    tag_add.execute(params![id, t])?;
                }
                for t in &remove_ids {
                    tag_rm.execute(params![id, t])?;
                }
            }
            Ok(ids.len())
        })
    }

    pub fn set_rating(&self, ids: &[FileId], rating: u8) -> Result<usize> {
        self.annotate(
            ids,
            &AnnotationChange {
                rating: Some(rating),
                ..Default::default()
            },
        )
    }

    pub fn set_label(&self, ids: &[FileId], label: Option<ColorLabel>) -> Result<usize> {
        self.annotate(
            ids,
            &AnnotationChange {
                label: Some(label),
                ..Default::default()
            },
        )
    }

    pub fn set_pick(&self, ids: &[FileId], pick: PickState) -> Result<usize> {
        self.annotate(
            ids,
            &AnnotationChange {
                pick: Some(pick),
                ..Default::default()
            },
        )
    }

    pub fn set_notes(&self, ids: &[FileId], notes: Option<&str>) -> Result<usize> {
        self.annotate(
            ids,
            &AnnotationChange {
                notes: Some(notes.map(str::to_string)),
                ..Default::default()
            },
        )
    }

    pub fn add_tags(&self, ids: &[FileId], tags: &[&str]) -> Result<usize> {
        self.annotate(
            ids,
            &AnnotationChange {
                add_tags: tags.iter().map(|s| s.to_string()).collect(),
                ..Default::default()
            },
        )
    }

    pub fn remove_tags(&self, ids: &[FileId], tags: &[&str]) -> Result<usize> {
        self.annotate(
            ids,
            &AnnotationChange {
                remove_tags: tags.iter().map(|s| s.to_string()).collect(),
                ..Default::default()
            },
        )
    }

    pub fn annotation(&self, id: FileId) -> Result<Annotation> {
        self.with_conn(|c| {
            let mut a = c
                .query_row(
                    "SELECT rating, label, pick, notes FROM annotations WHERE file_id = ?1",
                    [id],
                    |r| {
                        Ok(Annotation {
                            rating: r.get::<_, i64>(0)?.clamp(0, 5) as u8,
                            label: r
                                .get::<_, Option<String>>(1)?
                                .and_then(|s| ColorLabel::parse(&s)),
                            pick: PickState::from_i64(r.get(2)?),
                            notes: r.get(3)?,
                            tags: Vec::new(),
                        })
                    },
                )
                .optional()?
                .unwrap_or_default();
            let mut st = c.prepare_cached(
                "SELECT t.name FROM file_tags ft JOIN tags t ON t.id = ft.tag_id
                 WHERE ft.file_id = ?1 ORDER BY t.name_lc",
            )?;
            a.tags = st
                .query_map([id], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            Ok(a)
        })
    }

    /// All tags with usage counts, most used first.
    pub fn tags(&self) -> Result<Vec<TagCount>> {
        self.with_conn(|c| {
            let mut st = c.prepare(
                "SELECT t.name, COUNT(ft.file_id) FROM tags t LEFT JOIN file_tags ft ON ft.tag_id = t.id
                 GROUP BY t.id ORDER BY 2 DESC, t.name_lc",
            )?;
            let v = st
                .query_map([], |r| Ok(TagCount { name: r.get(0)?, files: r.get::<_, i64>(1)? as u64 }))?
                .collect::<rusqlite::Result<_>>()?;
            Ok(v)
        })
    }

    /// Rename a tag; if `new` already exists the two tags are merged.
    pub fn rename_tag(&self, old: &str, new: &str) -> Result<()> {
        let new = clean_tag(new).ok_or_else(|| DbError::Invalid("tag name is empty".into()))?;
        if new.chars().count() > MAX_TAG_NAME_CHARS {
            return Err(DbError::Invalid(format!(
                "tag name exceeds {MAX_TAG_NAME_CHARS} characters"
            )));
        }
        self.with_tx(|tx| {
            let Some(old_id) = tag_id_tx(tx, old, false)? else {
                return Err(DbError::NotFound(format!("tag '{old}'")));
            };
            match tag_id_tx(tx, &new, false)? {
                Some(new_id) if new_id != old_id => {
                    tx.execute(
                        "INSERT OR IGNORE INTO file_tags(file_id, tag_id) SELECT file_id, ?2 FROM file_tags WHERE tag_id = ?1",
                        params![old_id, new_id],
                    )?;
                    tx.execute("DELETE FROM tags WHERE id = ?1", [old_id])?;
                }
                _ => {
                    tx.execute(
                        "UPDATE tags SET name = ?2, name_lc = ?3 WHERE id = ?1",
                        params![old_id, new, new.to_lowercase()],
                    )?;
                }
            }
            Ok(())
        })
    }

    pub fn delete_tag(&self, name: &str) -> Result<bool> {
        self.with_conn(|c| {
            Ok(c.execute(
                "DELETE FROM tags WHERE name_lc = ?1",
                [name.trim().to_lowercase()],
            )? > 0)
        })
    }
}

fn validated_tags(tags: &[String]) -> Result<Vec<String>> {
    tags.iter()
        .filter_map(|tag| clean_tag(tag))
        .map(|tag| {
            if tag.chars().count() > MAX_TAG_NAME_CHARS {
                Err(DbError::Invalid(format!(
                    "tag name exceeds {MAX_TAG_NAME_CHARS} characters"
                )))
            } else {
                Ok(tag)
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::files::NewFile;
    use forge_core::model::EntryKind;
    use std::path::PathBuf;

    fn files(db: &Db, n: usize) -> Vec<FileId> {
        let v: Vec<NewFile> = (0..n)
            .map(|i| NewFile {
                path: PathBuf::from(format!(r"C:\t\{i}.jpg")),
                kind: EntryKind::File,
                size: 1,
                mtime: None,
                ctime: None,
            })
            .collect();
        db.upsert_files(&v).unwrap()
    }

    #[test]
    fn batch_annotations() {
        let db = Db::open_in_memory().unwrap();
        let ids = files(&db, 3);
        db.annotate(
            &ids[..2],
            &AnnotationChange {
                rating: Some(4),
                label: Some(Some(ColorLabel::Green)),
                pick: Some(PickState::Pick),
                notes: Some(Some("hero shot".into())),
                add_tags: vec!["Wedding".into(), " smith ".into(), "".into()],
                ..Default::default()
            },
        )
        .unwrap();
        let a = db.annotation(ids[0]).unwrap();
        assert_eq!(a.rating, 4);
        assert_eq!(a.label, Some(ColorLabel::Green));
        assert_eq!(a.pick, PickState::Pick);
        assert_eq!(a.notes.as_deref(), Some("hero shot"));
        assert_eq!(a.tags, vec!["smith".to_string(), "Wedding".to_string()]);
        assert_eq!(db.annotation(ids[2]).unwrap(), Annotation::default());

        // Partial change leaves other fields alone; tags are case-insensitive.
        db.annotate(
            &ids[..1],
            &AnnotationChange {
                label: Some(None),
                remove_tags: vec!["WEDDING".into()],
                ..Default::default()
            },
        )
        .unwrap();
        let a = db.annotation(ids[0]).unwrap();
        assert_eq!(a.rating, 4);
        assert_eq!(a.label, None);
        assert_eq!(a.tags, vec!["smith".to_string()]);

        assert!(db.set_rating(&ids, 6).is_err());
        assert_eq!(
            db.annotation(ids[2]).unwrap().rating,
            0,
            "failed batch must not apply"
        );

        let t = db.tags().unwrap();
        assert_eq!(
            t[0],
            TagCount {
                name: "smith".into(),
                files: 2
            }
        );
        db.rename_tag("smith", "wedding").unwrap(); // merge into existing
        assert_eq!(
            db.tags().unwrap(),
            vec![TagCount {
                name: "Wedding".into(),
                files: 2
            }]
        );
        db.rename_tag("wedding", "Ceremony").unwrap();
        assert_eq!(
            db.annotation(ids[1]).unwrap().tags,
            vec!["Ceremony".to_string()]
        );
        assert!(db.delete_tag("ceremony").unwrap());
        assert!(db.annotation(ids[1]).unwrap().tags.is_empty());
    }
}
