//! Embedded adapters for the Sidequery owned-tool contract.
use rmcp::model::{Tool, ToolAnnotations};
use serde_json::{json, Value};

#[derive(Clone, Copy)]
pub enum Operation {
    Query { read_only: bool },
    Schema,
    List,
    SourceRead,
    SourceWrite,
    Live(&'static str),
}

pub struct Definition {
    pub tool: Tool,
    pub operation: Operation,
}

fn definition(
    name: &'static str,
    title: &str,
    description: &str,
    risk: &str,
    operation: Operation,
    properties: Value,
    required: Value,
) -> Definition {
    let schema = json!({"type":"object", "properties":properties, "required":required, "additionalProperties":false});
    let read_only = risk == "read_only";
    let mut tool = Tool::new(name, description.to_owned(), schema.as_object().unwrap().clone())
        .with_title(title)
        .with_annotations(ToolAnnotations::from_raw(None, Some(read_only), Some(risk == "destructive"), Some(read_only), Some(true)))
        .with_meta(rmcp::model::MetaObject(json!({"dev.sidequery/contract":{"canonicalId":name,"wireName":name,"version":"2.0","risk":risk}}).as_object().unwrap().clone()));
    tool.output_schema = Some(std::sync::Arc::new(
        json!({"type":"object","required":["ok"],"properties":{"ok":{"type":"boolean"}}})
            .as_object()
            .unwrap()
            .clone(),
    ));
    Definition { tool, operation }
}

pub fn definitions() -> Vec<Definition> {
    let text = json!({"type":"string"});
    let limit = json!({"type":"integer","minimum":1,"maximum":1000,"default":100});
    let sql = json!({"sql":text,"limit":limit,"max_rows":{"type":"integer","minimum":1,"maximum":1000,"description":"Hosted spelling of limit; do not provide conflicting values."}});
    let mut tools = vec![
        definition("sidequery.sql.query", "Query SQL", "Execute one read-only SQL statement on the persistent agent DuckDB connection, separate from browser temporary tables and transactions. Results are bounded to 1000 rows and 1 MiB; deadline 30 seconds. Use execute for mutations.", "read_only", Operation::Query { read_only: true }, sql.clone(), json!(["sql"])),
        definition("sidequery.sql.execute", "Execute SQL", "Execute exactly one SQL statement, including DDL and DML, on the persistent agent connection. This can change data. Results are bounded to 1000 rows and 1 MiB; deadline 30 seconds.", "destructive", Operation::Query { read_only: false }, sql, json!(["sql"])),
        definition("sidequery.sql.schema.inspect", "Inspect SQL Schema", "List schema columns visible to the agent connection, subject to query result bounds.", "read_only", Operation::Schema, json!({}), json!([])),
        definition("sidequery.notebook.sql.list", "List SQL Notebooks", "List saved notebook paths and connected browser sessions with their current revision. Use session_id to disambiguate multiple windows; edits and runs require expected_revision.", "read_only", Operation::List, json!({}), json!([])),
        definition("sidequery.notebook.sql.source.read", "Read Saved SQL Notebook", "Read saved notebook source and revision. Use notebook get for the live, unsaved browser document.", "read_only", Operation::SourceRead, json!({"notebook_path":text}), json!(["notebook_path"])),
        definition("sidequery.notebook.sql.source.write", "Write Saved SQL Notebook", "Write saved notebook source with compare-and-swap. Empty expected_revision creates a new notebook. Rejects paths currently open in a connected browser; live edits remain unsaved.", "destructive", Operation::SourceWrite, json!({"notebook_path":text,"source":text,"expected_revision":text}), json!(["notebook_path","source","expected_revision"])),
    ];
    for (name, title, operation, risk) in [
        (
            "sidequery.notebook.sql.get",
            "Get SQL Notebook",
            "notebook.get",
            "read_only",
        ),
        (
            "sidequery.notebook.sql.cell.list",
            "List SQL Cells",
            "cell.list",
            "read_only",
        ),
        (
            "sidequery.notebook.sql.run",
            "Run SQL Notebook",
            "notebook.run",
            "destructive",
        ),
        (
            "sidequery.notebook.sql.cell.run",
            "Run SQL Cell",
            "cell.run",
            "destructive",
        ),
        (
            "sidequery.notebook.sql.cell.insert",
            "Insert SQL Cell",
            "cell.insert",
            "additive",
        ),
        (
            "sidequery.notebook.sql.cell.update",
            "Update SQL Cell",
            "cell.update",
            "destructive",
        ),
        (
            "sidequery.notebook.sql.cell.delete",
            "Delete SQL Cell",
            "cell.delete",
            "destructive",
        ),
    ] {
        let mut props = json!({"notebook_path":text,"session_id":text,"expected_revision":text})
            .as_object()
            .unwrap()
            .clone();
        if operation != "cell.list" && operation != "cell.delete" {
            props.insert("limit".into(), limit.clone());
        }
        if operation == "notebook.get" {
            props.insert(
                "run".into(),
                json!({"enum":["none","missing","all"],"default":"none"}),
            );
            props.insert(
                "cell_limit".into(),
                json!({"type":"integer","minimum":1,"maximum":3}),
            );
        }
        if ["notebook.get", "notebook.run", "cell.run"].contains(&operation) {
            props.insert(
                "bust_cache".into(),
                json!({"type":"boolean","default":false}),
            );
        }
        if operation.starts_with("cell.") && !["cell.insert", "cell.list"].contains(&operation) {
            props.insert("cell_id".into(), text.clone());
            props.insert("cell_index".into(), json!({"type":"integer","minimum":0}));
        }
        if ["cell.insert", "cell.update"].contains(&operation) {
            props.insert("sql".into(), text.clone());
            props.insert("title".into(), text.clone());
            props.insert("connection".into(), json!({"type":["string","null"]}));
            props.insert("markdown".into(), json!({"type":["string","null"]}));
            props.insert(
                "run".into(),
                json!({"enum":["none","cell","affected"],"default":"none"}),
            );
        }
        if operation == "cell.insert" {
            props.insert("index".into(), json!({"type":"integer","minimum":0}));
        }
        let mut required = vec![];
        if risk != "read_only" {
            required.push("expected_revision");
        }
        if operation == "cell.insert" {
            required.push("sql");
        }
        tools.push(definition(name, title, "Operate on the connected browser notebook using the shared Sidequery notebook arguments and result envelope. notebook_path selects a unique open document; omit it only when one session exists. session_id disambiguates windows. Edits remain unsaved (mutation.saved=false). Reads and edits default to run=none; explicit run commands permit mutations. Automatic read-only execution modes may be refused when the browser cannot enforce them. Cancellation stops subsequent cells but cannot interrupt an active browser query.", risk, Operation::Live(operation), Value::Object(props), json!(required)));
    }
    tools
}

/// Validate the deliberately small schema vocabulary used above before dispatch.
pub fn validate(tool: &Tool, args: &Value) -> Result<(), String> {
    let args = args
        .as_object()
        .ok_or("INVALID_ARGUMENT: arguments must be an object")?;
    let properties = tool.input_schema["properties"].as_object().unwrap();
    for key in tool.input_schema["required"].as_array().unwrap() {
        if !args.contains_key(key.as_str().unwrap()) {
            return Err(format!(
                "INVALID_ARGUMENT: {} is required",
                key.as_str().unwrap()
            ));
        }
    }
    for (key, value) in args {
        let schema = properties
            .get(key)
            .ok_or_else(|| format!("INVALID_ARGUMENT: unknown argument {key}"))?;
        let valid_type = match schema.get("type").and_then(Value::as_str) {
            Some("string") => value.is_string(),
            Some("integer") => value.as_u64().is_some(),
            Some("boolean") => value.is_boolean(),
            _ => schema.get("type").is_none_or(|kind| {
                kind.as_array().is_some_and(|types| {
                    (types.contains(&json!("string")) && value.is_string())
                        || (types.contains(&json!("null")) && value.is_null())
                })
            }),
        };
        if !valid_type
            || schema
                .get("enum")
                .is_some_and(|choices| !choices.as_array().unwrap().contains(value))
            || schema
                .get("minimum")
                .is_some_and(|min| value.as_u64() < min.as_u64())
            || schema
                .get("maximum")
                .is_some_and(|max| value.as_u64() > max.as_u64())
        {
            return Err(format!("INVALID_ARGUMENT: invalid {key}"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_has_complete_owned_metadata_and_closed_schemas() {
        let tools = definitions();
        let names: Vec<_> = tools
            .iter()
            .map(|definition| definition.tool.name.as_ref())
            .collect();
        assert_eq!(
            names,
            vec![
                "sidequery.sql.query",
                "sidequery.sql.execute",
                "sidequery.sql.schema.inspect",
                "sidequery.notebook.sql.list",
                "sidequery.notebook.sql.source.read",
                "sidequery.notebook.sql.source.write",
                "sidequery.notebook.sql.get",
                "sidequery.notebook.sql.cell.list",
                "sidequery.notebook.sql.run",
                "sidequery.notebook.sql.cell.run",
                "sidequery.notebook.sql.cell.insert",
                "sidequery.notebook.sql.cell.update",
                "sidequery.notebook.sql.cell.delete",
            ]
        );
        for definition in tools {
            let wire = serde_json::to_value(&definition.tool).unwrap();
            assert_eq!(
                wire["_meta"]["dev.sidequery/contract"]["canonicalId"],
                wire["name"]
            );
            assert_eq!(
                wire["_meta"]["dev.sidequery/contract"]["wireName"],
                wire["name"]
            );
            assert_eq!(wire["_meta"]["dev.sidequery/contract"]["version"], "2.0");
            assert!(wire["title"].is_string());
            assert_eq!(wire["inputSchema"]["additionalProperties"], false);
            assert_eq!(wire["outputSchema"]["required"], json!(["ok"]));
            for hint in [
                "readOnlyHint",
                "destructiveHint",
                "idempotentHint",
                "openWorldHint",
            ] {
                assert!(wire["annotations"][hint].is_boolean());
            }
            if let Operation::Query { read_only } = definition.operation {
                assert_eq!(wire["annotations"]["readOnlyHint"], read_only);
                assert_eq!(wire["annotations"]["destructiveHint"], !read_only);
            }
        }
    }
}
