//! Every field the chart's schema accepts has to reach something.
//!
//! The failure this exists to stop has happened twice: a field added to
//! `values.schema.json` that no template renders is accepted by Helm, written
//! nowhere, and silently does nothing. The `actions` block shipped that way
//! once, and `jobNamesPath` would have shipped that way in 0.4.6.
//!
//! `test-chart-config.sh` already renders the chart and loads it with the
//! collector's parser, but it asserts on a fixture, so it only covers fields
//! someone remembered to add to it. Both misses got past it for exactly that
//! reason. This enumerates the schema instead, so a new field is covered by
//! existing it.

use serde_json::Value;
use std::collections::BTreeSet;

const SCHEMA: &str = include_str!("../charts/kartero/values.schema.json");

/// Fields the schema declares but nothing is expected to render.
///
/// Each needs a reason. An entry here is a deliberate exception, not a place
/// to silence this test.
const NOT_RENDERED: &[&str] = &[];

fn leaves(node: &Value, path: &str, out: &mut BTreeSet<String>) {
    if let Some(props) = node.get("properties").and_then(Value::as_object) {
        for (name, child) in props {
            let child_path = if path.is_empty() {
                name.clone()
            } else {
                format!("{path}/{name}")
            };
            let kind = child.get("type").and_then(Value::as_str).unwrap_or("");
            if matches!(kind, "string" | "integer" | "number" | "boolean" | "array") {
                out.insert(child_path.clone());
            }
            leaves(child, &child_path, out);
        }
    }
    if let Some(items) = node.get("items") {
        leaves(items, &format!("{path}[]"), out);
    }
}

/// Read every template and helper, not just `templates/*.yaml`: `image.tag` is
/// referenced from `_helpers.tpl`, and missing that file would report a gap
/// that is not one.
fn all_template_text() -> String {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/charts/kartero/templates");
    let mut text = String::new();
    for entry in std::fs::read_dir(dir).expect("templates directory") {
        let path = entry.expect("template entry").path();
        if path.is_file() {
            text.push_str(&std::fs::read_to_string(&path).expect("template file"));
            text.push('\n');
        }
    }
    text
}

#[test]
fn every_field_the_schema_accepts_is_rendered_by_a_template() {
    let schema: Value = serde_json::from_str(SCHEMA).expect("values.schema.json parses");
    let mut fields = BTreeSet::new();
    leaves(&schema, "", &mut fields);
    assert!(
        fields.len() > 30,
        "walked the schema and found only {} fields; the walk is wrong, not the chart",
        fields.len()
    );

    let templates = all_template_text();
    let exceptions: BTreeSet<&str> = NOT_RENDERED.iter().copied().collect();
    let unrendered: Vec<&String> = fields
        .iter()
        .filter(|field| {
            let name = field.rsplit('/').next().unwrap_or(field);
            !templates.contains(name) && !exceptions.contains(field.as_str())
        })
        .collect();

    assert!(
        unrendered.is_empty(),
        "these fields are accepted by the schema and rendered by nothing, so setting them \
         does nothing and says nothing: {unrendered:?}"
    );
}
