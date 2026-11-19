use serde::{Deserialize, Serialize};

fn deserialize_int_or_bool<'de, D>(deserializer: D) -> Result<bool, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum IntOrBool {
        Bool(bool),
        Int(u8),
    }

    match IntOrBool::deserialize(deserializer)? {
        IntOrBool::Bool(value) => Ok(value),
        IntOrBool::Int(0) => Ok(false),
        IntOrBool::Int(1) => Ok(true),
        IntOrBool::Int(_) => Err(serde::de::Error::custom("public must be 0, 1, or boolean")),
    }
}

/// Summary metadata for a Wallhaven collection returned by `/collections` or `/collections/{username}`.
///
/// Task 3 displays upstream names, counts, and visibility only; Favorites identification and
/// collection items belong to Task 6a.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CollectionSummary {
    pub id: u64,
    pub label: String,
    #[serde(default)]
    pub views: u64,
    #[serde(deserialize_with = "deserialize_int_or_bool")]
    pub public: bool,
    #[serde(default)]
    pub count: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_numeric_and_boolean_public_flags() {
        let numeric: CollectionSummary = serde_json::from_str(
            r#"{"id": 1, "label": "Default", "views": 0, "public": 1, "count": 5}"#,
        )
        .expect("numeric public=1");
        assert!(numeric.public);

        let private: CollectionSummary = serde_json::from_str(
            r#"{"id": 2, "label": "Private", "views": 0, "public": 0, "count": 3}"#,
        )
        .expect("numeric public=0");
        assert!(!private.public);

        let boolean: CollectionSummary = serde_json::from_str(
            r#"{"id": 3, "label": "Bool", "views": 10, "public": true, "count": 1}"#,
        )
        .expect("boolean public=true");
        assert!(boolean.public);

        let invalid: Result<CollectionSummary, _> = serde_json::from_str(
            r#"{"id": 4, "label": "Bad", "views": 0, "public": 2, "count": 0}"#,
        );
        assert!(invalid.is_err());
    }
}
