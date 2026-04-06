//! Data-driven stage prompts compiled into the binary.
//!
//! Prompts are defined in prompts/stage_prompts.json and compiled into the
//! binary via include_str!. This allows editing prompts without changing
//! Rust code - just edit the JSON and recompile.

use serde::Deserialize;
use std::collections::HashMap;

/// Raw prompt template with placeholder variables like {title}, {repo}, {plan}.
#[derive(Deserialize)]
struct PromptEntry {
    template: String,
}

/// All stage prompts loaded from the compiled-in JSON.
static PROMPTS_JSON: &str = include_str!("../prompts/stage_prompts.json");

/// Get a prompt template by key and render it with the given variables.
///
/// Variables are replaced using simple {key} substitution.
/// Unknown keys in the template are left as-is.
pub fn render(key: &str, vars: &HashMap<&str, &str>) -> Option<String> {
    let prompts: HashMap<String, PromptEntry> =
        serde_json::from_str(PROMPTS_JSON).expect("prompts/stage_prompts.json must be valid JSON");
    let entry = prompts.get(key)?;
    let mut result = entry.template.clone();
    for (k, v) in vars {
        result = result.replace(&format!("{{{k}}}"), v);
    }
    Some(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_planning_prompt() {
        let mut vars = HashMap::new();
        vars.insert("title", "Fix auth bug");
        vars.insert("repo", "/path/to/repo");
        let result = render("planning", &vars).unwrap();
        assert!(result.contains("Fix auth bug"));
        assert!(result.contains("/path/to/repo"));
        assert!(result.contains("workbridge_set_plan"));
    }

    #[test]
    fn render_implementing_with_plan() {
        let mut vars = HashMap::new();
        vars.insert("title", "Add feature");
        vars.insert("repo", "/repo");
        vars.insert("plan", "Step 1: do thing\nStep 2: test");
        let result = render("implementing_with_plan", &vars).unwrap();
        assert!(result.contains("Step 1: do thing"));
        assert!(result.contains("workbridge_set_status"));
    }

    #[test]
    fn render_implementing_no_plan() {
        let mut vars = HashMap::new();
        vars.insert("title", "Add feature");
        vars.insert("repo", "/repo");
        let result = render("implementing_no_plan", &vars).unwrap();
        assert!(result.contains("CRITICAL: No implementation plan"));
        assert!(result.contains("Blocked"));
    }

    #[test]
    fn unknown_key_returns_none() {
        let vars = HashMap::new();
        assert!(render("nonexistent_prompt", &vars).is_none());
    }

    #[test]
    fn all_prompt_keys_valid() {
        let prompts: HashMap<String, PromptEntry> = serde_json::from_str(PROMPTS_JSON).unwrap();
        assert!(prompts.contains_key("planning"));
        assert!(prompts.contains_key("implementing_with_plan"));
        assert!(prompts.contains_key("implementing_no_plan"));
        assert!(prompts.contains_key("implementing_rework"));
        assert!(prompts.contains_key("blocked"));
        assert!(prompts.contains_key("review"));
        assert!(prompts.contains_key("review_gate"));
    }

    #[test]
    fn all_prompts_prohibit_git_config() {
        let prompts: HashMap<String, PromptEntry> = serde_json::from_str(PROMPTS_JSON).unwrap();
        let prohibition = "NEVER run 'git config' to set any values";
        for (key, entry) in &prompts {
            assert!(
                entry.template.contains(prohibition),
                "prompt '{}' is missing git config prohibition",
                key
            );
        }
    }

    #[test]
    fn render_implementing_rework() {
        let mut vars = HashMap::new();
        vars.insert("title", "Fix auth bug");
        vars.insert("repo", "/path/to/repo");
        vars.insert("plan", "Step 1: implement\nStep 2: test");
        vars.insert("rework_reason", "Tests are failing on CI");
        let result = render("implementing_rework", &vars).unwrap();
        assert!(result.contains("Fix auth bug"));
        assert!(result.contains("Tests are failing on CI"));
        assert!(result.contains("rework"));
        assert!(result.contains("workbridge_set_status"));
    }
}
