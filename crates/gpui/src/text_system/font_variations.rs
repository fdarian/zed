use std::borrow::Cow;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use schemars::{JsonSchema, json_schema};

/// The values of the variation axes of a variable font, as in CSS
/// `font-variation-settings`. An axis is named by its four-character tag, read as
/// a big-endian `u32` (`wght` is `0x77676874`).
///
/// An axis the font does not have is ignored, and a value is clamped to the range
/// of its axis. The list is sorted by tag and names a tag once, so two settings
/// with the same values are equal.
#[derive(Default, Clone, PartialEq)]
pub struct FontVariations(Arc<Vec<(u32, f32)>>);

impl Eq for FontVariations {}

impl Hash for FontVariations {
    fn hash<H: Hasher>(&self, state: &mut H) {
        for (tag, value) in self.0.iter() {
            tag.hash(state);
            value.to_bits().hash(state);
        }
    }
}

impl FontVariations {
    /// The settings in `variations`. A tag that appears twice takes its last value, as in CSS.
    pub fn new(variations: impl IntoIterator<Item = (u32, f32)>) -> Self {
        let mut merged: Vec<(u32, f32)> = Vec::new();
        for (tag, value) in variations {
            match merged.iter_mut().find(|(existing, _)| *existing == tag) {
                Some(entry) => entry.1 = value,
                None => merged.push((tag, value)),
            }
        }
        merged.sort_by_key(|(tag, _)| *tag);
        Self(Arc::new(merged))
    }

    /// The tag of a four-byte axis name such as `wght`.
    pub fn tag(name: [u8; 4]) -> u32 {
        u32::from_be_bytes(name)
    }

    /// The value set for the axis `tag`, if any.
    pub fn get(&self, tag: u32) -> Option<f32> {
        self.0
            .iter()
            .find(|(existing, _)| *existing == tag)
            .map(|(_, value)| *value)
    }

    /// The settings, sorted by tag.
    pub fn tag_value_list(&self) -> &[(u32, f32)] {
        self.0.as_slice()
    }

    /// Whether no axis is set.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

fn tag_name(tag: u32) -> String {
    tag.to_be_bytes().iter().map(|byte| *byte as char).collect()
}

impl std::fmt::Debug for FontVariations {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut debug = f.debug_struct("FontVariations");
        for (tag, value) in self.tag_value_list() {
            debug.field(&tag_name(*tag), value);
        }
        debug.finish()
    }
}

impl<'de> serde::Deserialize<'de> for FontVariations {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error;

        let map = <std::collections::BTreeMap<String, f32> as serde::Deserialize>::deserialize(
            deserializer,
        )?;
        let mut variations = Vec::with_capacity(map.len());
        for (name, value) in map {
            let tag: [u8; 4] = name
                .as_bytes()
                .try_into()
                .map_err(|_| D::Error::custom(format!("incorrect font variation tag: {name}")))?;
            variations.push((Self::tag(tag), value));
        }
        Ok(Self::new(variations))
    }
}

impl serde::Serialize for FontVariations {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeMap;

        let mut map = serializer.serialize_map(Some(self.0.len()))?;
        for (tag, value) in self.tag_value_list() {
            map.serialize_entry(&tag_name(*tag), value)?;
        }
        map.end()
    }
}

impl JsonSchema for FontVariations {
    fn schema_name() -> Cow<'static, str> {
        "FontVariations".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        json_schema!({
            "type": "object",
            "patternProperties": {
                "^[\\x20-\\x7e]{4}$": {
                    "type": "number"
                }
            },
            "additionalProperties": false
        })
    }
}
