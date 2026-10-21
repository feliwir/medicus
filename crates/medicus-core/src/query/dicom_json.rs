//! [`Attributes`] over the DICOM JSON model (PS3.18 Annex F).

use std::borrow::Cow;

use serde_json::{Map, Value};

use super::Attributes;
use crate::Tag;

/// A DICOM JSON dataset (`{"GGGGEEEE": {"vr": .., "Value": [..]}, ..}`) viewed as [`Attributes`].
#[derive(Debug, Clone, Copy)]
pub struct DicomJson<'a>(&'a Map<String, Value>);

impl<'a> DicomJson<'a> {
    pub fn new(dataset: &'a Map<String, Value>) -> Self {
        Self(dataset)
    }
}

impl Attributes for DicomJson<'_> {
    fn values(&self, tag: Tag) -> Option<Vec<Cow<'_, str>>> {
        Attributes::values(self.0, tag)
    }

    fn items(&self, tag: Tag) -> Vec<&dyn Attributes> {
        Attributes::items(self.0, tag)
    }
}

fn element(map: &Map<String, Value>, tag: Tag) -> Option<&Map<String, Value>> {
    map.get(&String::from(tag)).and_then(Value::as_object)
}

/// Person names are objects with up to three component groups (F.2.2).
fn person_name(obj: &Map<String, Value>) -> String {
    let group = |k: &str| obj.get(k).and_then(Value::as_str).unwrap_or("");
    let groups = [group("Alphabetic"), group("Ideographic"), group("Phonetic")];
    groups.join("=").trim_end_matches('=').to_owned()
}

impl Attributes for Map<String, Value> {
    fn values(&self, tag: Tag) -> Option<Vec<Cow<'_, str>>> {
        let el = element(self, tag)?;
        let Some(Value::Array(values)) = el.get("Value") else {
            return Some(Vec::new());
        };
        Some(
            values
                .iter()
                .filter_map(|v| match v {
                    Value::String(s) => Some(Cow::Borrowed(s.as_str())),
                    Value::Number(n) => Some(Cow::Owned(n.to_string())),
                    Value::Object(o) => Some(Cow::Owned(person_name(o))),
                    _ => None,
                })
                .collect(),
        )
    }

    fn items(&self, tag: Tag) -> Vec<&dyn Attributes> {
        let Some(Value::Array(items)) = element(self, tag).and_then(|el| el.get("Value")) else {
            return Vec::new();
        };
        items
            .iter()
            .filter_map(Value::as_object)
            .map(|o| o as &dyn Attributes)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_values() {
        let v = json!({
            "00100010": { "vr": "PN", "Value": [{ "Alphabetic": "Yamada^Tarou", "Ideographic": "山田^太郎" }] },
            "00280010": { "vr": "US", "Value": [512] },
            "00081030": { "vr": "LO" }
        });
        let d = DicomJson::new(v.as_object().unwrap());
        assert_eq!(
            d.values(Tag::new(0x0010, 0x0010)).unwrap(),
            vec!["Yamada^Tarou=山田^太郎"]
        );
        assert_eq!(d.values(Tag::new(0x0028, 0x0010)).unwrap(), vec!["512"]);
        assert_eq!(
            d.values(Tag::new(0x0008, 0x1030)).unwrap(),
            Vec::<Cow<'_, str>>::new()
        );
        assert!(d.values(Tag::new(0x0008, 0x0050)).is_none());
    }
}
