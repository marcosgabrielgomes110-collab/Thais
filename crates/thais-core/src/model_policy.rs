//! Session model policy: a default model plus ordered role fallbacks.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Default plus role-based model candidates.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelPolicy {
    /// Default model.
    #[serde(default)]
    pub default: String,
    /// Ordered candidates per role.
    #[serde(default)]
    pub roles: BTreeMap<String, Vec<String>>,
}

impl ModelPolicy {
    /// Parse a policy from the session JSON value.
    #[must_use]
    pub fn from_value(value: &Value) -> Self {
        let default = value
            .get("default")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let mut roles = BTreeMap::new();
        if let Some(object) = value.get("roles").and_then(Value::as_object) {
            for (role, candidates) in object {
                let models = match candidates {
                    Value::String(model) => vec![model.clone()],
                    Value::Array(models) => models
                        .iter()
                        .filter_map(Value::as_str)
                        .map(ToOwned::to_owned)
                        .collect(),
                    _ => Vec::new(),
                };
                if !models.is_empty() {
                    roles.insert(role.clone(), models);
                }
            }
        }
        Self { default, roles }
    }

    /// Render the policy as JSON for storage.
    #[must_use]
    pub fn to_value(&self) -> Value {
        json!({"default": self.default, "roles": self.roles})
    }

    /// Set a role to one model. `default` changes the default.
    pub fn set_role(&mut self, role: &str, model: &str) {
        if role == "default" {
            self.default = model.to_owned();
        } else {
            self.roles.insert(role.to_owned(), vec![model.to_owned()]);
        }
    }

    /// Ordered candidates for a role. Falls back to the default model.
    #[must_use]
    pub fn candidates(&self, role: &str) -> Vec<String> {
        let mut candidates = self.roles.get(role).cloned().unwrap_or_default();
        if candidates.is_empty() && !self.default.is_empty() {
            candidates.push(self.default.clone());
        }
        candidates
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_strings_and_arrays() {
        let policy = ModelPolicy::from_value(&json!({
            "default": "base",
            "roles": {
                "plan": ["p1", "p2"],
                "build": "b1"
            }
        }));
        assert_eq!(policy.default, "base");
        assert_eq!(policy.candidates("plan"), ["p1", "p2"]);
        assert_eq!(policy.candidates("build"), ["b1"]);
        assert_eq!(policy.candidates("general"), ["base"]);
    }

    #[test]
    fn set_role_updates_default_and_roles() {
        let mut policy = ModelPolicy::default();
        policy.set_role("default", "base");
        policy.set_role("plan", "planner");
        assert_eq!(policy.default, "base");
        assert_eq!(policy.candidates("plan"), ["planner"]);
        assert_eq!(policy.candidates("build"), ["base"]);
    }
}
