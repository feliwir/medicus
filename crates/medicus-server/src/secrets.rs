//! Secrets in addon configurations (ADR-0005 §7). An addon's configuration schema marks secret
//! fields `"writeOnly": true`. The admin API never returns their values: it shows
//! [`placeholder`] instead, and a placeholder sent back keeps the stored value.
//!
//! Values are matched by their path in the configuration: object keys, and array positions.

use std::fmt::Write as _;

use serde_json::{Value, json};

/// What the admin API shows in place of a secret.
pub(crate) fn placeholder() -> Value {
    json!({ "$secret": true })
}

fn is_unset(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::String(s) => s.is_empty(),
        Value::Array(a) => a.is_empty(),
        Value::Object(o) => o.is_empty(),
        _ => false,
    }
}

fn is_placeholder(value: &Value) -> bool {
    *value == placeholder()
}

/// Replaces the secrets in `config` with [`placeholder`]s. Unset ones (`null`, or empty) stay as
/// they are.
pub(crate) fn mask(schema: &Value, config: &mut Value) {
    // Masking cannot fail.
    let _ = visit(schema, schema, config, None, &mut String::new(), Mode::Mask);
}

/// Replaces the [`placeholder`]s in `config` with the values at the same paths in `stored`.
/// Fails with the path of a placeholder that has no stored value.
pub(crate) fn restore(schema: &Value, config: &mut Value, stored: &Value) -> Result<(), String> {
    visit(
        schema,
        schema,
        config,
        Some(stored),
        &mut String::new(),
        Mode::Restore,
    )
}

/// Whether `config` contains a string that looks like a URL with a password, outside the secret
/// fields. Returns its path.
pub(crate) fn password_in_url(schema: &Value, config: &Value) -> Option<String> {
    let mut config = config.clone();
    mask(schema, &mut config);
    find_password(&config, &mut String::new())
}

fn find_password(value: &Value, path: &mut String) -> Option<String> {
    match value {
        Value::String(s) => has_password(s).then(|| path.clone()),
        Value::Object(map) => map.iter().find_map(|(k, v)| {
            let len = path.len();
            path.push('/');
            path.push_str(k);
            let found = find_password(v, path);
            path.truncate(len);
            found
        }),
        Value::Array(items) => items.iter().enumerate().find_map(|(i, v)| {
            let len = path.len();
            let _ = write!(path, "/{i}");
            let found = find_password(v, path);
            path.truncate(len);
            found
        }),
        _ => None,
    }
}

/// `scheme://user:password@host...`
fn has_password(s: &str) -> bool {
    let Some((scheme, rest)) = s.split_once("://") else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    !scheme.is_empty()
        && scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c))
        && authority
            .rsplit_once('@')
            .is_some_and(|(userinfo, _)| userinfo.contains(':'))
}

#[derive(Clone, Copy)]
enum Mode {
    Mask,
    Restore,
}

/// `schema` with a local `$ref` (`#/...`) followed.
fn resolve<'a>(root: &'a Value, mut schema: &'a Value) -> &'a Value {
    // Bounded, against reference cycles.
    for _ in 0..32 {
        let Some(target) = schema
            .get("$ref")
            .and_then(Value::as_str)
            .and_then(|r| r.strip_prefix('#'))
            .and_then(|pointer| root.pointer(pointer))
        else {
            break;
        };
        schema = target;
    }
    schema
}

fn visit(
    root: &Value,
    schema: &Value,
    value: &mut Value,
    stored: Option<&Value>,
    path: &mut String,
    mode: Mode,
) -> Result<(), String> {
    let schema = resolve(root, schema);
    if schema.get("writeOnly") == Some(&Value::Bool(true)) {
        match mode {
            Mode::Mask if !is_unset(value) => *value = placeholder(),
            Mode::Restore if is_placeholder(value) => {
                *value = stored.filter(|s| !s.is_null()).cloned().ok_or_else(|| {
                    format!("the secret at {path:?} is kept, but there is no stored value to keep")
                })?;
            }
            _ => {}
        }
        return Ok(());
    }
    // `Option<T>` and other alternatives: each may mark the value.
    for key in ["allOf", "anyOf", "oneOf"] {
        for sub in schema
            .get(key)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            visit(root, sub, value, stored, path, mode)?;
        }
    }
    match value {
        Value::Object(map) => {
            for (k, v) in map.iter_mut() {
                let sub = schema
                    .get("properties")
                    .and_then(|p| p.get(k))
                    .or_else(|| schema.get("additionalProperties").filter(|a| a.is_object()));
                if let Some(sub) = sub {
                    let len = path.len();
                    path.push('/');
                    path.push_str(k);
                    visit(root, sub, v, stored.and_then(|s| s.get(k)), path, mode)?;
                    path.truncate(len);
                }
            }
        }
        Value::Array(items) => {
            if let Some(sub) = schema.get("items") {
                for (i, v) in items.iter_mut().enumerate() {
                    let len = path.len();
                    let _ = write!(path, "/{i}");
                    visit(root, sub, v, stored.and_then(|s| s.get(i)), path, mode)?;
                    path.truncate(len);
                }
            }
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shape schemars gives a list of structs with an optional secret and a secret map.
    fn schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "url": { "type": "string" },
                "password": { "type": ["string", "null"], "writeOnly": true },
                "hooks": { "type": "array", "items": { "$ref": "#/$defs/Hook" } },
            },
            "$defs": {
                "Hook": {
                    "type": "object",
                    "properties": {
                        "name": { "type": "string" },
                        "secret": { "anyOf": [{ "$ref": "#/$defs/Secret" }, { "type": "null" }] },
                        "headers": {
                            "type": "object",
                            "additionalProperties": { "type": "string" },
                            "writeOnly": true,
                        },
                    },
                },
                "Secret": { "type": "string", "writeOnly": true },
            },
        })
    }

    fn config() -> Value {
        json!({
            "url": "postgres://db/medicus",
            "password": "pw",
            "hooks": [
                { "name": "a", "secret": "s-a", "headers": { "Authorization": "Bearer x" } },
                { "name": "b", "secret": null, "headers": {} },
            ],
        })
    }

    #[test]
    fn secrets_are_masked() {
        let mut masked = config();
        mask(&schema(), &mut masked);
        assert_eq!(
            masked,
            json!({
                "url": "postgres://db/medicus",
                "password": { "$secret": true },
                "hooks": [
                    { "name": "a", "secret": { "$secret": true }, "headers": { "$secret": true } },
                    { "name": "b", "secret": null, "headers": {} },
                ],
            })
        );
    }

    #[test]
    fn placeholders_keep_the_stored_values() {
        let mut changed = config();
        mask(&schema(), &mut changed);
        changed["hooks"][0]["name"] = json!("renamed");
        changed["password"] = json!("new");
        restore(&schema(), &mut changed, &config()).unwrap();
        let mut expected = config();
        expected["hooks"][0]["name"] = json!("renamed");
        expected["password"] = json!("new");
        assert_eq!(changed, expected);

        // A placeholder without a stored value is refused.
        let mut added = config();
        added["hooks"][1]["secret"] = placeholder();
        let err = restore(&schema(), &mut added, &config()).unwrap_err();
        assert!(err.contains("/hooks/1/secret"), "{err}");
    }

    #[test]
    fn passwords_in_urls_are_found_outside_secrets() {
        let schema = schema();
        assert_eq!(password_in_url(&schema, &config()), None);
        let mut c = config();
        c["url"] = json!("postgres://medicus:pw@db/medicus");
        assert_eq!(password_in_url(&schema, &c).as_deref(), Some("/url"));
        c["url"] = json!("postgres://medicus@db/medicus?x=a:b@c");
        assert_eq!(password_in_url(&schema, &c), None);
        // Inside a secret, a URL may carry anything.
        let mut c = config();
        c["password"] = json!("https://u:p@x");
        assert_eq!(password_in_url(&schema, &c), None);
    }
}
