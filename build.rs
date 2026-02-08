//! This build script merges the schemas from sm-json-data to a single "total" schema, that can
//! then be fed to the Typify crate to generate corresponding Rust types.
//!
//! To accomplish this, the following steps are taken:
//! * Read all the schemas in the provided directory. It is assumed their names follow the pattern
//!   `"m3-some-name.schema.json"`.
//! * Strip any constructs unsupported by Typify. Currently, this is only if/then/else.
//! * Rewrite any JSON `"$ref"` references to work with the merged schema.
//! * Extract any "definitions" from the schemas, moving them to the total schema.
//! * Add whatever remains of the schemas as definitions, using names based on the schema file name
//!   (e.g. `SchemaSomeName`).

#![feature(strip_circumfix)]

use serde_json::{Map, Value as JsonValue, json};
use std::collections::HashMap;
use std::env;
use std::path::PathBuf;

fn main() {
    // Resolve the sm-json-data schema directory, or default to our own submodule
    let schema_dir: PathBuf = env::var("SM_JSON_DATA_SCHEMA_DIR").map_or(
        PathBuf::from("sm-json-data/schema").canonicalize().unwrap(),
        |v| {
            PathBuf::from(v)
                .canonicalize()
                .expect("Invalid path SM_JSON_DATA_SCHEMA_DIR")
        },
    );
    let schema_dir = schema_dir.to_str().unwrap();

    // Tell cargo to rerun this script if anything changes in schema_dir
    println!("cargo::rerun-if-changed={}", schema_dir);

    // Pairs of schema filenames and their names in the total schema.
    // The names serve as names for the corresponding structs/enums, if any.
    let schema_lookup: HashMap<String, String> = std::fs::read_dir(&schema_dir)
        .unwrap_or_else(|_| panic!("Could not read schema directory '{}'", schema_dir))
        .filter_map(|res| res.ok().map(|e| e.path()))
        .filter(|e| e.is_file())
        .filter_map(|e| {
            e.file_name()
                .and_then(|n| n.to_str())
                .map(|s| s.to_string())
        })
        .filter_map(|s| {
            s.strip_circumfix("m3-", ".schema.json")
                .map(|name| (s.clone(), schema_name_to_type_name(name)))
        })
        .collect();

    // Definitions extracted from the schemas
    let mut definitions = Map::new();

    // Mapping from schema names to whatever remains after definitions have been extracted,
    // references have been rewritten, and unsupported constructs have been removed.
    let schemas: Map<_, _> = schema_lookup
        .iter()
        .map(|(filename, name)| (name, read_json(&format!("{}/{}", schema_dir, filename))))
        .map(|(name, mut schema)| (name, strip_if_then_else(&mut schema)))
        .map(|(name, mut schema)| {
            rewrite_references(name, &mut schema, &schema_lookup);
            (name, schema)
        })
        .map(|(name, mut schema)| {
            extract_definitions(&mut definitions, &mut schema);
            (name.to_string(), schema)
        })
        .collect();

    // Now add what remains in the schemas to the definitions, so that Typify generates types for
    // them.
    definitions.extend(schemas);

    let total_schema = json!({
      "$schema": "http://json-schema.org/draft-07/schema#",
      "definitions": definitions,
    });

    std::fs::write(
        "generated/m3-total.schema.json",
        serde_json::to_string_pretty(&total_schema).unwrap(),
    )
    .unwrap();
}

fn read_json(path: &str) -> JsonValue {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// Uppercase the first character of a string
fn uppercase_first(s: &str) -> String {
    let mut iter = s.chars();
    match iter.next() {
        None => String::new(),
        Some(c) => c.to_uppercase().chain(iter).collect(),
    }
}

/// Convert the name part of a schema file name to a type name that will be used in the schema.
/// This turns "foo-bar-baz" into "SchemaFooBarBaz".
fn schema_name_to_type_name(s: &str) -> String {
    s.split("-")
        .map(uppercase_first)
        .chain(["Schema".to_string()])
        .collect::<Vec<_>>()
        .join("")
}

/// Rewrite JSON $ref instances to work with the merged schema.
fn rewrite_references(name: &str, value: &mut JsonValue, schema_lookup: &HashMap<String, String>) {
    fn fix_reference(
        reference: &str,
        name: &str,
        schema_lookup: &HashMap<String, String>,
    ) -> String {
        if reference.starts_with("#/properties/") {
            // Since we are moving the schemas to definitions, this has to change
            format!("#/definitions/{}/{}", name, &reference[2..])
        } else if let Some(idx) = reference.find("#")
            && idx > 0
        {
            // This is a reference to one of the other schemas, which will be merged, so
            // strip the schema name.
            reference[idx..].to_string()
        } else if !reference.contains("#") {
            // This is a reference to an entire schema by name. Since it will move to
            // definitions, refer to its name there instead.
            if let Some(schema) = schema_lookup.get(reference) {
                format!("#/definitions/{}", schema)
            } else {
                panic!("Did not find schema for $ref '{}'", reference);
            }
        } else {
            reference.to_string()
        }
    }

    if value.is_object() {
        value
            .as_object_mut()
            .unwrap()
            .iter_mut()
            .for_each(|(k, v)| {
                if k == "$ref" {
                    *v = fix_reference(v.as_str().unwrap(), name, schema_lookup).into();
                } else {
                    rewrite_references(name, v, schema_lookup);
                }
            });
    } else if value.is_array() {
        value
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .for_each(|v| rewrite_references(name, v, schema_lookup));
    }
}

/// Extract anything found in the "definitions" key of a schema
fn extract_definitions(definitions: &mut Map<String, JsonValue>, schema: &mut JsonValue) {
    if let JsonValue::Object(object) = schema
        && let Some(JsonValue::Object(defs)) = object.remove("definitions")
    {
        definitions.extend(defs);
    }
}

/// Strip any instances of if/then/else constructs in a schema.
///
/// The if/then/else construct is not supported by Typify. The only current use in sm-json-data is
/// to specify whether properties are required or not, and we can live without that.
fn strip_if_then_else(value: &mut JsonValue) -> JsonValue {
    fn is_if_then_else(v: &JsonValue) -> bool {
        v.is_object() && {
            let o = v.as_object().unwrap();
            o.contains_key("if") && o.contains_key("then")
        }
    }

    match value {
        JsonValue::Object(map) => map
            .iter_mut()
            .filter(|(_k, v)| !is_if_then_else(v))
            .map(|(k, v)| (k.to_owned(), strip_if_then_else(v)))
            .collect::<Map<_, _>>()
            .into(),
        JsonValue::Array(vec) => vec
            .iter_mut()
            .filter(|v| !is_if_then_else(v))
            .map(strip_if_then_else)
            .collect::<Vec<_>>()
            .into(),
        _ => value.clone(),
    }
}
