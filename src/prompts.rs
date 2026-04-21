//! Data-driven stage prompts compiled into the binary.
//!
//! Prompts are defined in prompts/stage_prompts.json and compiled into the
//! binary via include_str!. This allows editing prompts without changing
//! Rust code - just edit the JSON and recompile.

use std::collections::HashMap;

use serde::Deserialize;

/// Raw prompt template with placeholder variables like {title}, {situation}, {plan}.
#[derive(Deserialize)]
struct PromptEntry {
    template: String,
}

/// All stage prompts loaded from the compiled-in JSON.
static PROMPTS_JSON: &str = include_str!("../prompts/stage_prompts.json");

/// Get a prompt template by key and render it with the given variables.
///
/// Variables are replaced using single-pass {key} substitution.
/// This ensures that substituted values are never re-scanned for further
/// template markers, preventing user-supplied text from injecting variables.
/// Unknown keys in the template are left as-is.
pub fn render(key: &str, vars: &HashMap<&str, &str>) -> Option<String> {
    let prompts: HashMap<String, PromptEntry> =
        serde_json::from_str(PROMPTS_JSON).expect("prompts/stage_prompts.json must be valid JSON");
    let entry = prompts.get(key)?;
    Some(render_template(&entry.template, vars))
}

/// Single-pass template renderer. Scans the template for `{key}` markers and
/// replaces them with values from `vars`. Substituted values are appended
/// verbatim and never re-scanned, so user-supplied text cannot inject template
/// variables.
fn render_template(template: &str, vars: &HashMap<&str, &str>) -> String {
    let mut result = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        // Append everything before the '{'
        result.push_str(&rest[..open]);
        let after_open = &rest[open + 1..];
        if let Some(close) = after_open.find('}') {
            let key = &after_open[..close];
            if let Some(value) = vars.get(key) {
                // Known variable - substitute without re-scanning
                result.push_str(value);
            } else {
                // Unknown variable - leave the marker as-is
                result.push('{');
                result.push_str(key);
                result.push('}');
            }
            rest = &after_open[close + 1..];
        } else {
            // No closing brace - append the '{' and continue
            result.push('{');
            rest = after_open;
        }
    }
    // Append any remaining text after the last marker
    result.push_str(rest);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_planning_prompt() {
        let mut vars = HashMap::new();
        vars.insert("title", "Fix auth bug");
        vars.insert(
            "situation",
            "Worktree: /tmp/wt. Branch: fix-auth. No plan exists yet.",
        );
        vars.insert("description", "");
        let result = render("planning", &vars).unwrap();
        assert!(result.contains("Fix auth bug"));
        assert!(result.contains("Worktree: /tmp/wt"));
        assert!(result.contains("workbridge_set_plan"));
        assert!(result.contains("Acceptance Criteria"));
        assert!(result.contains("PHASE 1: REFINEMENT"));
        assert!(result.contains("PHASE 2: PLANNING"));
    }

    #[test]
    fn render_planning_quickstart_prompt() {
        let mut vars = HashMap::new();
        vars.insert(
            "situation",
            "Worktree: /tmp/wt. Branch: user/quickstart-ab12. No plan exists yet.",
        );
        let result = render("planning_quickstart", &vars).unwrap();
        assert!(result.contains("workbridge_set_title"));
        assert!(result.contains("workbridge_set_plan"));
        assert!(result.contains("PHASE 0"));
        assert!(result.contains("PHASE 1: REFINEMENT"));
        assert!(result.contains("PHASE 2: PLANNING"));
        assert!(result.contains("Acceptance Criteria"));
    }

    #[test]
    fn render_implementing_with_plan() {
        let mut vars = HashMap::new();
        vars.insert("title", "Add feature");
        vars.insert("situation", "Worktree: /tmp/wt. Branch: add-feature.");
        vars.insert("plan", "Step 1: do thing\nStep 2: test");
        vars.insert("description", "");
        let result = render("implementing_with_plan", &vars).unwrap();
        assert!(result.contains("Step 1: do thing"));
        assert!(result.contains("workbridge_set_status"));
    }

    #[test]
    fn render_implementing_no_plan() {
        let mut vars = HashMap::new();
        vars.insert("title", "Add feature");
        vars.insert("situation", "Worktree: /tmp/wt. Branch: add-feature.");
        vars.insert("description", "");
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
        assert!(prompts.contains_key("planning_quickstart"));
        assert!(prompts.contains_key("implementing_with_plan"));
        assert!(prompts.contains_key("implementing_no_plan"));
        assert!(prompts.contains_key("implementing_rework"));
        assert!(prompts.contains_key("blocked"));
        assert!(prompts.contains_key("review"));
        assert!(prompts.contains_key("review_with_findings"));
        assert!(prompts.contains_key("review_gate"));
        assert!(prompts.contains_key("global_assistant"));
        // Auto-start messages (C7 in docs/harness-contract.md) live in
        // the same JSON so they can be edited without recompiling.
        assert!(prompts.contains_key("auto_start_default"));
        assert!(prompts.contains_key("auto_start_review"));
    }

    /// Auto-start messages (`auto_start_*` keys) are short literal user
    /// prompts, not system prompts. The git-* prohibitions belong in the
    /// system prompt (which already carries them for every stage via
    /// `stage_system_prompt`), so the auto-start keys are deliberately
    /// excluded from the prohibition checks below. This matches the
    /// harness contract: auto-start is C7 (initial user message), the
    /// prohibitions are part of C6 (system prompt content).
    fn is_auto_start_key(key: &str) -> bool {
        key.starts_with("auto_start_")
    }

    #[test]
    fn all_prompts_prohibit_git_config() {
        let prompts: HashMap<String, PromptEntry> = serde_json::from_str(PROMPTS_JSON).unwrap();
        let prohibition = "NEVER run 'git config' to set any values";
        for (key, entry) in &prompts {
            if is_auto_start_key(key) {
                continue;
            }
            assert!(
                entry.template.contains(prohibition),
                "prompt '{}' is missing git config prohibition",
                key
            );
        }
    }

    #[test]
    fn all_prompts_prohibit_git_checkout() {
        let prompts: HashMap<String, PromptEntry> = serde_json::from_str(PROMPTS_JSON).unwrap();
        let prohibition = "NEVER run 'git checkout'";
        for (key, entry) in &prompts {
            if is_auto_start_key(key) {
                continue;
            }
            assert!(
                entry.template.contains(prohibition),
                "prompt '{}' is missing git checkout prohibition",
                key
            );
        }
    }

    #[test]
    fn render_implementing_rework() {
        let mut vars = HashMap::new();
        vars.insert("title", "Fix auth bug");
        vars.insert(
            "situation",
            "Worktree: /tmp/wt. Branch: fix-auth. Rework requested.",
        );
        vars.insert("plan", "Step 1: implement\nStep 2: test");
        vars.insert("rework_reason", "Tests are failing on CI");
        vars.insert("description", "");
        let result = render("implementing_rework", &vars).unwrap();
        assert!(result.contains("Fix auth bug"));
        assert!(result.contains("Tests are failing on CI"));
        assert!(result.contains("rework"));
        assert!(result.contains("workbridge_set_status"));
    }

    #[test]
    fn render_no_unsubstituted_markers() {
        // Verify that when all known variables are provided, no {key} markers remain.
        let cases = vec![
            (
                "planning_quickstart",
                vec![(
                    "situation",
                    "Worktree: /tmp/wt. Branch: user/quickstart-ab12.",
                )],
            ),
            (
                "planning",
                vec![("title", "Test"), ("situation", "Sit"), ("description", "")],
            ),
            (
                "implementing_with_plan",
                vec![
                    ("title", "Test"),
                    ("situation", "Sit"),
                    ("plan", "Plan"),
                    ("description", ""),
                ],
            ),
            (
                "implementing_no_plan",
                vec![("title", "Test"), ("situation", "Sit"), ("description", "")],
            ),
            (
                "blocked",
                vec![("title", "Test"), ("situation", "Sit"), ("description", "")],
            ),
            (
                "review",
                vec![("title", "Test"), ("situation", "Sit"), ("description", "")],
            ),
            (
                "review_with_findings",
                vec![
                    ("title", "Test"),
                    ("situation", "Sit"),
                    ("review_gate_findings", "All plan items implemented"),
                    ("description", ""),
                ],
            ),
            (
                "implementing_rework",
                vec![
                    ("title", "Test"),
                    ("situation", "Sit"),
                    ("plan", "Plan"),
                    ("rework_reason", "Reason"),
                    ("description", ""),
                ],
            ),
            (
                "review_gate",
                vec![
                    ("repo_path", "/tmp/repo"),
                    ("default_branch", "main"),
                    ("branch", "feature/test"),
                ],
            ),
            ("global_assistant", vec![("repo_list", "- /tmp/repo")]),
        ];
        for (key, var_list) in cases {
            let vars: HashMap<&str, &str> = var_list.into_iter().collect();
            let result = render(key, &vars).unwrap();
            // Check no unsubstituted {word} markers remain
            let mut rest = result.as_str();
            while let Some(open) = rest.find('{') {
                let after = &rest[open + 1..];
                if let Some(close) = after.find('}') {
                    let inner = &after[..close];
                    // Allow JSON-like braces or empty braces, but flag template vars
                    assert!(
                        inner.contains(' ') || inner.contains(':') || inner.is_empty(),
                        "prompt '{}' has unsubstituted marker: {{{}}}",
                        key,
                        inner
                    );
                    rest = &after[close + 1..];
                } else {
                    break;
                }
            }
        }
    }

    #[test]
    fn render_global_assistant() {
        let mut vars = HashMap::new();
        vars.insert(
            "repo_list",
            "- /Users/foo/project-a\n- /Users/foo/project-b",
        );
        let result = render("global_assistant", &vars).unwrap();
        assert!(result.contains("cross-project assistant"));
        assert!(result.contains("/Users/foo/project-a"));
        assert!(result.contains("workbridge_list_repos"));
        assert!(result.contains("workbridge_create_work_item"));
    }

    #[test]
    fn all_interactive_prompts_have_interaction_policy() {
        let prompts: HashMap<String, PromptEntry> = serde_json::from_str(PROMPTS_JSON).unwrap();
        let sentinel = "INTERACTION POLICY";
        let interactive = [
            "planning_quickstart",
            "planning",
            "planning_retroactive",
            "implementing_with_plan",
            "implementing_no_plan",
            "implementing_rework",
            "blocked",
            "review",
            "review_with_findings",
            "global_assistant",
        ];
        for key in interactive {
            let entry = prompts
                .get(key)
                .unwrap_or_else(|| panic!("missing prompt key: {key}"));
            assert!(
                entry.template.contains(sentinel),
                "prompt '{key}' is missing the INTERACTION POLICY block"
            );
        }
        // review_gate is headless; the policy would be misleading there.
        let gate = prompts
            .get("review_gate")
            .expect("review_gate prompt must exist");
        assert!(
            !gate.template.contains(sentinel),
            "review_gate must NOT reference INTERACTION POLICY (it is headless)"
        );
    }

    /// Pins harness parity: every interactive system prompt MUST open
    /// with a `HARNESS DIRECTIVE OVERRIDE` block that explicitly
    /// neutralises harness-specific defaults (e.g. Codex's baked-in
    /// "assume the user wants the work done unless they explicitly
    /// ask for a plan" prior). Without this block, Codex follows its
    /// built-in prior instead of the workbridge prompt and jumps to
    /// implementation in planning sessions.
    ///
    /// RCA (2026-04-18): a user reported that Codex went straight to
    /// implementation on a work item that should have been in a
    /// blocked-no-plan state. Codex's response quoted its own
    /// "operating instructions" about assuming the user wants the
    /// work done. The workbridge prompts were descriptive ("You are
    /// a planning assistant") rather than imperative, so Codex's
    /// prior won. Claude's training weights descriptive system
    /// prompts more heavily, masking the issue. Fix: open every
    /// interactive prompt with `HARNESS DIRECTIVE OVERRIDE:` and
    /// explicit MUST / MUST NOT wording so both harnesses read the
    /// same directive.
    #[test]
    fn all_interactive_prompts_have_harness_directive_override() {
        let prompts: HashMap<String, PromptEntry> = serde_json::from_str(PROMPTS_JSON).unwrap();
        let sentinel = "HARNESS DIRECTIVE OVERRIDE";
        let interactive = [
            "planning_quickstart",
            "planning",
            "planning_retroactive",
            "implementing_with_plan",
            "implementing_no_plan",
            "implementing_rework",
            "blocked",
            "review",
            "review_with_findings",
            "global_assistant",
        ];
        for key in interactive {
            let entry = prompts
                .get(key)
                .unwrap_or_else(|| panic!("missing prompt key: {key}"));
            assert!(
                entry.template.starts_with(sentinel),
                "prompt '{key}' must OPEN with a `{sentinel}` block so both harnesses \
                 read the same imperative before their baked-in priors kick in; got \
                 first 60 chars: {:?}",
                entry.template.chars().take(60).collect::<String>()
            );
        }
        // review_gate is headless + JSON-only; the override block
        // would add no value there and would pollute the JSON-schema
        // hand-off. It is intentionally exempt.
        let gate = prompts
            .get("review_gate")
            .expect("review_gate prompt must exist");
        assert!(
            !gate.template.contains(sentinel),
            "review_gate must NOT carry a HARNESS DIRECTIVE OVERRIDE block \
             (headless, no harness defaults to override)"
        );
    }

    /// Pins that the auto-start messages (C7) are NOT interpreted
    /// as concrete implementation requests. Both `auto_start_default`
    /// and `auto_start_review` were tightened 2026-04-18 to tell the
    /// harness to follow its system prompt's first action, rather
    /// than the previous ambiguous "Explain who you are and start
    /// working." which Codex's prior read as "just start implementing".
    #[test]
    fn auto_start_messages_defer_to_system_prompt() {
        let prompts: HashMap<String, PromptEntry> = serde_json::from_str(PROMPTS_JSON).unwrap();
        for key in ["auto_start_default", "auto_start_review"] {
            let entry = prompts
                .get(key)
                .unwrap_or_else(|| panic!("missing prompt key: {key}"));
            let lower = entry.template.to_lowercase();
            assert!(
                lower.contains("system prompt") || lower.contains("system-prompt"),
                "auto-start '{key}' must reference the system prompt as the source of truth; got {:?}",
                entry.template
            );
        }
    }

    #[test]
    fn description_with_template_markers_not_expanded() {
        // User-supplied description containing {plan} must not cause plan variable injection
        let mut vars = HashMap::new();
        vars.insert("title", "Test");
        vars.insert("situation", "Sit");
        vars.insert("plan", "Step 1: real plan");
        vars.insert(
            "description",
            "\nUser-provided description: {plan} is my text",
        );
        let result = render("implementing_with_plan", &vars).unwrap();
        // The description should appear literally with {plan} intact, not expanded
        assert!(
            result.contains("{plan} is my text"),
            "description's {{plan}} was expanded - injection vulnerability"
        );
        // The actual plan variable should still be substituted in its proper location
        assert!(result.contains("Step 1: real plan"));
    }
}
