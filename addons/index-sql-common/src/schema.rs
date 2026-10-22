//! Mapping of the standard attributes (`medicus_core::tags::STANDARD`) to tables and columns.
//!
//! Table aliases used in all generated SQL: `p` patients, `st` studies, `se` series,
//! `i` instances.

use medicus_core::{QueryLevel, Tag, VR, tags};

/// How an attribute is stored and therefore matched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Storage {
    /// Normalized text, exact (case-sensitive) matching.
    Text,
    /// Person name: normalized text plus a case-folded copy for case-insensitive matching.
    PersonName { folded: &'static str },
    /// DA in sortable `YYYYMMDD` form.
    Date,
    /// TM: the normalized value for return, plus its sortable lower bound for matching.
    Time { key: &'static str },
    /// IS stored as an integer.
    Int,
    /// A multi-valued study attribute derived from lower levels: the distinct values of
    /// `m.<column>` over `from`, a `FROM ... WHERE` fragment correlated with `st` (alias `m`).
    StudyMulti { from: &'static str },
    /// An integer computed on demand by a correlated subquery (`NumberOf...Related...`).
    Count { expr: &'static str },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Column {
    pub tag: Tag,
    pub vr: VR,
    pub level: QueryLevel,
    /// Table alias.
    pub alias: &'static str,
    pub column: &'static str,
    pub storage: Storage,
}

impl Column {
    /// `alias.column`
    pub fn expr(&self) -> String {
        format!("{}.{}", self.alias, self.column)
    }
}

macro_rules! columns {
    ($($tag:ident: $level:ident $alias:literal . $col:literal $storage:expr;)*) => {
        pub const COLUMNS: &[Column] = &[
            $(Column {
                tag: tags::$tag,
                vr: match tags::standard_vr(tags::$tag) { Some(vr) => vr, None => VR::UN },
                level: QueryLevel::$level,
                alias: $alias,
                column: $col,
                storage: $storage,
            },)*
        ];
    };
}

use Storage::{Count, Date, Int, PersonName, StudyMulti, Text, Time};

columns! {
    PATIENT_NAME: Patient "p"."patient_name" PersonName { folded: "patient_name_folded" };
    PATIENT_ID: Patient "p"."patient_id" Text;
    ISSUER_OF_PATIENT_ID: Patient "p"."issuer" Text;
    PATIENT_BIRTH_DATE: Patient "p"."birth_date" Date;
    PATIENT_SEX: Patient "p"."sex" Text;
    NUMBER_OF_PATIENT_RELATED_STUDIES: Patient "p"."id" Count {
        expr: "(SELECT COUNT(*) FROM studies c WHERE c.patient_fk = p.id)"
    };

    STUDY_INSTANCE_UID: Study "st"."study_uid" Text;
    STUDY_DATE: Study "st"."study_date" Date;
    STUDY_TIME: Study "st"."study_time" Time { key: "study_time_key" };
    ACCESSION_NUMBER: Study "st"."accession" Text;
    STUDY_ID: Study "st"."study_id" Text;
    REFERRING_PHYSICIAN_NAME: Study "st"."referring" PersonName { folded: "referring_folded" };
    STUDY_DESCRIPTION: Study "st"."description" Text;
    INSTITUTION_NAME: Study "st"."institution" Text;
    MODALITIES_IN_STUDY: Study "m"."modality" StudyMulti { from: "series m WHERE m.study_fk = st.id" };
    SOP_CLASSES_IN_STUDY: Study "m"."sop_class" StudyMulti {
        from: "instances m JOIN series ms ON ms.id = m.series_fk WHERE ms.study_fk = st.id"
    };
    NUMBER_OF_STUDY_RELATED_SERIES: Study "st"."id" Count {
        expr: "(SELECT COUNT(*) FROM series c WHERE c.study_fk = st.id)"
    };
    NUMBER_OF_STUDY_RELATED_INSTANCES: Study "st"."id" Count {
        expr: "(SELECT COUNT(*) FROM instances c JOIN series cs ON cs.id = c.series_fk WHERE cs.study_fk = st.id)"
    };

    SERIES_INSTANCE_UID: Series "se"."series_uid" Text;
    MODALITY: Series "se"."modality" Text;
    SERIES_NUMBER: Series "se"."series_number" Int;
    SERIES_DESCRIPTION: Series "se"."description" Text;
    SERIES_DATE: Series "se"."series_date" Date;
    SERIES_TIME: Series "se"."series_time" Time { key: "series_time_key" };
    BODY_PART_EXAMINED: Series "se"."body_part" Text;
    STATION_NAME: Series "se"."station" Text;
    PERFORMED_PROCEDURE_STEP_START_DATE: Series "se"."pps_start_date" Date;
    NUMBER_OF_SERIES_RELATED_INSTANCES: Series "se"."id" Count {
        expr: "(SELECT COUNT(*) FROM instances c WHERE c.series_fk = se.id)"
    };

    SOP_INSTANCE_UID: Instance "i"."sop_uid" Text;
    SOP_CLASS_UID: Instance "i"."sop_class" Text;
    INSTANCE_NUMBER: Instance "i"."instance_number" Int;
    CONTENT_DATE: Instance "i"."content_date" Date;
    CONTENT_TIME: Instance "i"."content_time" Time { key: "content_time_key" };
    NUMBER_OF_FRAMES: Instance "i"."num_frames" Int;
}

pub fn column(tag: Tag) -> Option<&'static Column> {
    COLUMNS.iter().find(|c| c.tag == tag)
}

/// Attributes computed by the index (counts, aggregates); never taken from instances.
pub fn is_computed(c: &Column) -> bool {
    matches!(c.storage, StudyMulti { .. } | Count { .. })
}

/// Correlated subquery for the total stored size of the level's row (for ordering by size).
pub fn size_expr(level: QueryLevel) -> Option<&'static str> {
    match level {
        QueryLevel::Patient => None,
        QueryLevel::Study => Some(
            "(SELECT COALESCE(SUM(c.size), 0) FROM instances c JOIN series cs ON cs.id = c.series_fk \
             WHERE cs.study_fk = st.id)",
        ),
        QueryLevel::Series => {
            Some("(SELECT COALESCE(SUM(c.size), 0) FROM instances c WHERE c.series_fk = se.id)")
        }
        QueryLevel::Instance => Some("i.size"),
    }
}

/// `FROM` clause for a query level, joining all parent levels.
pub fn from_clause(level: QueryLevel) -> &'static str {
    match level {
        QueryLevel::Patient => " FROM patients p",
        QueryLevel::Study => " FROM studies st JOIN patients p ON p.id = st.patient_fk",
        QueryLevel::Series => {
            " FROM series se JOIN studies st ON st.id = se.study_fk JOIN patients p ON p.id = st.patient_fk"
        }
        QueryLevel::Instance => {
            " FROM instances i JOIN series se ON se.id = i.series_fk JOIN studies st ON st.id = se.study_fk \
             JOIN patients p ON p.id = st.patient_fk"
        }
    }
}

/// Primary key expression of the level's table.
pub fn id_expr(level: QueryLevel) -> &'static str {
    match level {
        QueryLevel::Patient => "p.id",
        QueryLevel::Study => "st.id",
        QueryLevel::Series => "se.id",
        QueryLevel::Instance => "i.id",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_standard_attribute_is_mapped_once_with_its_vr_and_level() {
        for a in tags::STANDARD {
            let name = medicus_core::tag::Named(a.tag);
            let c = column(a.tag).unwrap_or_else(|| panic!("{name} not mapped"));
            assert_eq!((c.vr, c.level), (a.vr, a.level), "{name}");
        }
        assert_eq!(COLUMNS.len(), tags::STANDARD.len());
    }
}
