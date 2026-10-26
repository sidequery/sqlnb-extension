//! The local DuckDB UI's stored notebook format. Keep unknown metadata intact.
use serde_json::Value;
use std::collections::HashSet;

const MAX_BYTES: usize = 16 * 1024 * 1024;

fn notebook(source: &str) -> Result<Value, String> {
    if source.len() > MAX_BYTES {
        return Err("DuckDB UI notebook exceeds the 16 MiB limit".into());
    }
    let value: Value = serde_json::from_str(source)
        .map_err(|error| format!("Invalid DuckDB UI notebook JSON: {error}"))?;
    if value
        .get("notebookSerializationFormat")
        .and_then(Value::as_u64)
        != Some(3)
    {
        return Err("Unsupported DuckDB UI notebook format; expected format 3".into());
    }
    if !value
        .get("version")
        .and_then(Value::as_i64)
        .is_some_and(|version| (1..=i32::MAX as i64).contains(&version))
    {
        return Err("Invalid DuckDB UI notebook content version".into());
    }
    if !matches!(
        value.get("currentDatabase"),
        Some(Value::Null | Value::String(_))
    ) {
        return Err("Invalid DuckDB UI current database".into());
    }
    let view = value
        .get("viewMode")
        .and_then(Value::as_object)
        .ok_or("Invalid DuckDB UI notebook view mode")?;
    match view.get("mode").and_then(Value::as_str) {
        Some("default") => {}
        Some("fullScreen")
            if view.get("cellId").is_some_and(Value::is_number)
                && matches!(
                    view.get("focus").and_then(Value::as_str),
                    Some("editor" | "results" | "split")
                ) => {}
        _ => return Err("Invalid DuckDB UI notebook view mode".into()),
    }
    let cells = value
        .get("cells")
        .and_then(Value::as_array)
        .ok_or("Invalid DuckDB UI notebook cells")?;
    let mut ids = HashSet::new();
    for cell in cells {
        let id = cell
            .get("cellId")
            .filter(|id| id.is_number())
            .ok_or("Invalid DuckDB UI cell ID")?;
        if !ids.insert(id.to_string()) {
            return Err("Duplicate DuckDB UI cell ID".into());
        }
        if !matches!(cell.get("query"), Some(Value::Null | Value::String(_)))
            || !matches!(
                cell.get("useDatabase"),
                None | Some(Value::Null | Value::String(_))
            )
            || !matches!(cell.get("isActive"), None | Some(Value::Bool(_)))
            || !matches!(
                cell.get("runMode").and_then(Value::as_str),
                None | Some("default" | "instant")
            )
            || cell.get("runMode").is_some_and(|mode| !mode.is_string())
        {
            return Err("Invalid DuckDB UI notebook cell".into());
        }
    }
    Ok(value)
}

pub(crate) fn initial_version(source: &str) -> Result<String, String> {
    let mut value = notebook(source)?;
    value["version"] = Value::from(1);
    serde_json::to_string(&value).map_err(|error| error.to_string())
}

pub(crate) fn next_version(previous: &str, edited: &str) -> Result<String, String> {
    let previous = notebook(previous)?;
    let mut edited = notebook(edited)?;
    if edited["version"] != previous["version"] {
        return Err("DuckDB UI notebook content version changed; reload before saving".into());
    }
    let version = previous["version"].as_i64().unwrap();
    if version == i32::MAX as i64 {
        return Err("DuckDB UI notebook content version is exhausted".into());
    }
    edited["version"] = Value::from(version + 1);
    let result = serde_json::to_string(&edited).map_err(|error| error.to_string())?;
    if result.len() > MAX_BYTES {
        return Err("DuckDB UI notebook exceeds the 16 MiB limit".into());
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fixture() -> Value {
        json!({"notebookSerializationFormat":3,"version":2,"currentDatabase":"analytics",
            "viewMode":{"mode":"default"},"futureNotebookOption":{"keep":true},
            "cells":[{"cellId":2,"query":"-- A database-aware cell\nselect answer, 'π' as label from sample;",
                "useDatabase":"analytics","isActive":true,"runMode":"instant","futureCellOption":42},
                {"cellId":3,"query":null,"useDatabase":null}]})
    }

    #[test]
    fn advances_official_content_version_without_losing_unknown_metadata() {
        let previous = fixture();
        let mut edited = previous.clone();
        edited["cells"][0]["query"] = json!("select 'edited π' as value;");
        let result: Value = serde_json::from_str(
            &next_version(&previous.to_string(), &edited.to_string()).unwrap(),
        )
        .unwrap();
        edited["version"] = json!(3);
        assert_eq!(result, edited);
    }

    #[test]
    fn rejects_incompatible_or_malformed_edits_before_a_write() {
        let previous = fixture().to_string();
        for (field, invalid) in [
            ("notebookSerializationFormat", json!(4)),
            ("version", json!(3)),
            ("currentDatabase", json!(42)),
            ("viewMode", json!({"mode":"unknown"})),
            ("cells", json!({})),
        ] {
            let mut edited = fixture();
            edited[field] = invalid;
            assert!(
                next_version(&previous, &edited.to_string()).is_err(),
                "{field}"
            );
        }
        for (field, invalid) in [
            ("query", json!(7)),
            ("cellId", json!("2")),
            ("useDatabase", json!(true)),
            ("isActive", json!("yes")),
            ("runMode", json!("unknown")),
        ] {
            let mut edited = fixture();
            edited["cells"][0][field] = invalid;
            assert!(
                next_version(&previous, &edited.to_string()).is_err(),
                "{field}"
            );
        }
        let mut duplicate = fixture();
        duplicate["cells"][1]["cellId"] = json!(2);
        assert!(next_version(&previous, &duplicate.to_string()).is_err());
        assert!(next_version(&previous, "{").is_err());
    }
}

#[cfg(test)]
mod creation_tests {
    use super::*;
    #[test]
    fn new_identity_resets_version_preserving_opaque_metadata() {
        let source = r#"{"notebookSerializationFormat":3,"version":9,"currentDatabase":null,"viewMode":{"mode":"default","future":42},"cells":[{"cellId":7,"query":"select 1","opaque":[1,2]}],"future":{"keep":true}}"#;
        let mut expected: Value = serde_json::from_str(source).unwrap();
        expected["version"] = Value::from(1);
        assert_eq!(
            serde_json::from_str::<Value>(&initial_version(source).unwrap()).unwrap(),
            expected
        );
        assert!(initial_version("{}").is_err());
    }
}
