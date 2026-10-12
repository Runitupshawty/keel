//! Swift declarations generated from the operation registry's inlined JSON schemas.
use serde_json::Value;
use std::fmt::Write;

fn name(value: &str) -> String {
    value
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(|s| format!("{}{}", s[..1].to_ascii_uppercase(), &s[1..]))
        .collect()
}

fn ty(s: &Value, label: &str, out: &mut String) -> String {
    assert!(s.get("$ref").is_none(), "SDK schemas must be inlined: {s}");
    for key in ["anyOf", "oneOf"] {
        if let Some(variants) = s.get(key).and_then(Value::as_array) {
            let non_null: Vec<_> = variants.iter().filter(|v| v["type"] != "null").collect();
            if non_null.len() == 1 {
                let inner = ty(non_null[0], label, out);
                return if non_null.len() != variants.len() {
                    format!("{inner}?")
                } else {
                    inner
                };
            }
            return "JSONValue".into();
        }
    }
    if let Some(types) = s.get("type").and_then(Value::as_array) {
        let non_null: Vec<_> = types.iter().filter(|t| *t != "null").collect();
        if non_null.len() == 1 {
            let mut inner = s.clone();
            inner["type"] = non_null[0].clone();
            let inner = ty(&inner, label, out);
            return if non_null.len() != types.len() {
                format!("{inner}?")
            } else {
                inner
            };
        }
        return "JSONValue".into();
    }
    if let Some(values) = s.get("enum").and_then(Value::as_array) {
        if values.iter().all(Value::is_string) {
            writeln!(out, "public enum {label}: String, Codable, Sendable {{").unwrap();
            for (i, value) in values.iter().enumerate() {
                writeln!(out, "    case value{i} = {value}").unwrap();
            }
            out.push_str("}\n\n");
            return label.into();
        }
        return "JSONValue".into();
    }
    if s.get("allOf").is_some() || s.get("const").is_some() {
        return "JSONValue".into();
    }
    match s.get("type").and_then(Value::as_str) {
        Some("boolean") => "Bool".into(),
        Some("integer") => {
            if s["minimum"].as_f64().is_some_and(|n| n >= 0.0)
                || s["format"].as_str().is_some_and(|f| f.starts_with('u'))
            {
                "UInt64".into()
            } else {
                "Int64".into()
            }
        }
        Some("number") => "Double".into(),
        Some("string") => "String".into(),
        Some("array") => {
            if s.get("prefixItems").is_some() {
                "[JSONValue]".into()
            } else {
                format!(
                    "[{}]",
                    s.get("items")
                        .map(|v| ty(v, &format!("{label}Item"), out))
                        .unwrap_or("JSONValue".into())
                )
            }
        }
        Some("object") => {
            let Some(props) = s.get("properties").and_then(Value::as_object) else {
                return format!(
                    "[String: {}]",
                    s.get("additionalProperties")
                        .map(|v| ty(v, &format!("{label}Value"), out))
                        .unwrap_or("JSONValue".into())
                );
            };
            // A dictionary with named fields cannot be represented by synthesized Codable.
            if s.get("additionalProperties")
                .is_some_and(|v| v != &Value::Bool(false))
            {
                return "[String: JSONValue]".into();
            }
            let mut fields = Vec::new();
            for (field, schema) in props {
                assert!(
                    field.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'),
                    "unsupported Swift field name {field}"
                );
                let mut field_ty = ty(schema, &format!("{label}{}", name(field)), out);
                let required = s["required"]
                    .as_array()
                    .is_some_and(|r| r.contains(&Value::String(field.clone())));
                if !required && !field_ty.ends_with('?') {
                    field_ty.push('?');
                }
                fields.push((field, field_ty));
            }
            writeln!(out, "public struct {label}: Codable, Sendable {{").unwrap();
            for (field, field_ty) in &fields {
                writeln!(out, "    public var `{field}`: {field_ty}").unwrap();
            }
            let params = fields
                .iter()
                .map(|(field, t)| {
                    format!(
                        "`{field}`: {t}{}",
                        if t.ends_with('?') { " = nil" } else { "" }
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            writeln!(out, "    public init({params}) {{").unwrap();
            for (field, _) in &fields {
                writeln!(out, "        self.`{field}` = `{field}`").unwrap();
            }
            out.push_str("    }\n}\n\n");
            label.into()
        }
        Some("null") | None => "JSONValue".into(),
        other => panic!("unsupported SDK schema type {other:?}: {s}"),
    }
}

/// Generate the complete SDK deterministically; no daemon or library is opened.
pub fn generate() -> String {
    let mut out = String::from(
        "// Generated by cargo run -p keel-api --example generate_swift. Do not edit.\n\nimport Foundation\n\n",
    );
    let mut fingerprint = blake3::Hasher::new();
    for op in crate::OPS {
        fingerprint.update(op.name.as_bytes());
        fingerprint.update(&serde_json::to_vec(&(op.params)()).unwrap());
        fingerprint.update(&serde_json::to_vec(&(op.result)()).unwrap());
    }
    writeln!(
        out,
        "// API schema fingerprint: {}\n",
        fingerprint.finalize().to_hex()
    )
    .unwrap();
    out.push_str(include_str!("swift_transport.txt"));
    out.push('\n');
    let mut methods = String::new();
    for op in crate::OPS {
        let label = name(op.name);
        let params = ty(&(op.params)(), &format!("{label}Params"), &mut out);
        let result = ty(&(op.result)(), &format!("{label}Result"), &mut out);
        writeln!(methods, "    /// {}", op.summary).unwrap();
        writeln!(methods, "    public func {}(_ params: {params}) async throws -> {result} {{\n        try await call({:?}, params: params, result: {result}.self)\n    }}", op.name.replace('.', "_"), op.name).unwrap();
    }
    out.push_str("extension KeelClient {\n");
    out.push_str(&methods);
    out.push_str("}\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_swift_client_matches_registry() {
        assert_eq!(
            generate(),
            include_str!("../../../sdk/swift/Keel.swift"),
            "run cargo run -p keel-api --example generate_swift"
        );
    }

    #[test]
    fn nullable_objects_and_dictionaries() {
        let mut out = String::new();
        let nullable = serde_json::json!({"anyOf":[
            {"type":"object", "properties":{"default":{"type":"string"}}, "required":["default"]},
            {"type":"null"}
        ]});
        assert_eq!(ty(&nullable, "Nullable", &mut out), "Nullable?");
        assert!(out.contains("public var `default`: String"));
        assert_eq!(
            ty(
                &serde_json::json!({"type":"object", "additionalProperties":{"type":"integer"}}),
                "Map",
                &mut out
            ),
            "[String: Int64]"
        );
        assert_eq!(
            ty(
                &serde_json::json!({"type":"array", "prefixItems":[{"type":"integer"},{"type":"string"}]}),
                "Tuple",
                &mut out
            ),
            "[JSONValue]"
        );
        assert_eq!(ty(&Value::Bool(true), "Any", &mut out), "JSONValue");
    }

    #[test]
    fn schema_shapes() {
        let mut out = String::new();
        let schema = serde_json::json!({"type":"object", "required":["count"], "properties": {
            "count":{"type":"integer", "minimum":0}, "signed":{"type":"integer"},
            "label":{"type":["string","null"]}, "kind":{"type":"string","enum":["copy","move"]}
        }, "additionalProperties":false});
        assert_eq!(ty(&schema, "Fixture", &mut out), "Fixture");
        assert!(out.contains("public var `count`: UInt64"));
        assert!(out.contains("public var `signed`: Int64?"));
        assert!(out.contains("`label`: String? = nil"));
        assert!(out.contains("case value0 = \"copy\""));
        assert_eq!(
            ty(
                &serde_json::json!({"oneOf":[{"type":"string"},{"type":"integer"}]}),
                "Union",
                &mut out
            ),
            "JSONValue"
        );
        assert_eq!(
            ty(
                &serde_json::json!({"type":"array","items":{"type":"boolean"}}),
                "List",
                &mut out
            ),
            "[Bool]"
        );
    }
}
