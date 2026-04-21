//! `workbridge config` subcommand handlers.

use std::str::FromStr;

use crate::agent_backend;
use crate::config::{self, FileConfigProvider};

pub fn handle_config_subcommand(args: &[String]) {
    match args.get(2).map(std::string::String::as_str) {
        Some("set") => handle_config_set(args),
        None => handle_config_show(),
        Some(unknown) => {
            eprintln!("Unknown config subcommand: {unknown}");
            eprintln!("Usage: workbridge config [set <key> <value>]");
            std::process::exit(1);
        }
    }
}

fn handle_config_show() {
    match config::config_path() {
        Ok(path) => {
            println!("Config file: {}", path.display());
            if path.exists() {
                let contents = std::fs::read_to_string(&path).unwrap_or_else(|e| {
                    eprintln!("Error reading config: {e}");
                    std::process::exit(1);
                });
                println!();
                print!("{contents}");
            } else {
                println!("(no config file yet)");
            }
        }
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    }
}

/// Outcome of the `config set` core routine. Kept as a value so the
/// unit tests below can assert the branch taken without shelling out
/// and without asserting on stdout/stderr text.
#[derive(Debug, PartialEq, Eq)]
pub enum ConfigSetOutcome {
    /// The key was set and the config was saved. Payload is the
    /// canonical name of the value for logging (same form as will
    /// round-trip through `config.toml`).
    Saved { key: String, value: String },
    /// The user passed a key we don't know about. The CLI wrapper
    /// prints a `workbridge config set` usage line and exits 1.
    UnknownKey(String),
    /// The user passed a value that didn't parse for the given key.
    InvalidValue {
        key: String,
        value: String,
        err: String,
    },
    /// Missing positional arguments.
    MissingArgs,
}

/// Core of `workbridge config set <key> <value>`. Pure in the sense
/// that it takes an explicit `ConfigProvider` (so tests can pass an
/// `InMemoryConfigProvider`) and does not touch global state. The CLI
/// wrapper in `handle_config_set` maps each outcome to `println!` /
/// `eprintln!` + exit-code semantics.
pub fn apply_config_set(
    provider: &dyn config::ConfigProvider,
    args: &[String],
) -> ConfigSetOutcome {
    // args[0] is the program name; args[1] == "config"; args[2] ==
    // "set". Key starts at args[3].
    let Some(key) = args.get(3) else {
        return ConfigSetOutcome::MissingArgs;
    };
    let Some(value) = args.get(4) else {
        return ConfigSetOutcome::MissingArgs;
    };

    match key.as_str() {
        "global-assistant-harness" => {
            // Validate the value before loading/mutating config so a
            // typo cannot half-apply.
            if let Err(e) = agent_backend::AgentBackendKind::from_str(value) {
                return ConfigSetOutcome::InvalidValue {
                    key: key.clone(),
                    value: value.clone(),
                    err: e.to_string(),
                };
            }
            let mut cfg = match provider.load() {
                Ok(c) => c,
                Err(e) => {
                    return ConfigSetOutcome::InvalidValue {
                        key: key.clone(),
                        value: value.clone(),
                        err: format!("load failed: {e}"),
                    };
                }
            };
            cfg.defaults.global_assistant_harness = Some(value.clone());
            if let Err(e) = provider.save(&cfg) {
                return ConfigSetOutcome::InvalidValue {
                    key: key.clone(),
                    value: value.clone(),
                    err: format!("save failed: {e}"),
                };
            }
            ConfigSetOutcome::Saved {
                key: key.clone(),
                value: value.clone(),
            }
        }
        other => ConfigSetOutcome::UnknownKey(other.to_string()),
    }
}

fn handle_config_set(args: &[String]) {
    let outcome = apply_config_set(&FileConfigProvider, args);
    match outcome {
        ConfigSetOutcome::Saved { key, value } => {
            println!("saved: {key} = {value}");
        }
        ConfigSetOutcome::UnknownKey(k) => {
            eprintln!("Unknown config key: {k}");
            eprintln!("Supported keys: global-assistant-harness");
            std::process::exit(1);
        }
        ConfigSetOutcome::InvalidValue { key, value, err } => {
            eprintln!("Error: invalid value '{value}' for key '{key}': {err}");
            std::process::exit(1);
        }
        ConfigSetOutcome::MissingArgs => {
            eprintln!("Usage: workbridge config set <key> <value>");
            eprintln!("Supported keys: global-assistant-harness");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ConfigProvider, InMemoryConfigProvider};

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(std::string::ToString::to_string).collect()
    }

    /// Pins the round-trip: `workbridge config set global-assistant-
    /// harness codex` loads -> mutates -> saves through the provider.
    #[test]
    fn config_set_global_assistant_harness_writes_config() {
        let provider = InMemoryConfigProvider::new();
        let args = argv(&[
            "workbridge",
            "config",
            "set",
            "global-assistant-harness",
            "codex",
        ]);
        let outcome = apply_config_set(&provider, &args);
        assert_eq!(
            outcome,
            ConfigSetOutcome::Saved {
                key: "global-assistant-harness".into(),
                value: "codex".into(),
            }
        );

        // Reload via the provider to confirm the value was persisted.
        let reloaded = provider.load().unwrap();
        assert_eq!(
            reloaded.defaults.global_assistant_harness.as_deref(),
            Some("codex")
        );
    }

    /// Pins that typos are rejected without touching the provider.
    #[test]
    fn config_set_rejects_unknown_harness_name() {
        let provider = InMemoryConfigProvider::new();
        // Seed an existing value so we can assert it survives a
        // failed `set`.
        let mut seed = config::Config::for_test();
        seed.defaults.global_assistant_harness = Some("claude".into());
        provider.save(&seed).unwrap();

        let args = argv(&[
            "workbridge",
            "config",
            "set",
            "global-assistant-harness",
            "gemini",
        ]);
        let outcome = apply_config_set(&provider, &args);
        assert!(
            matches!(outcome, ConfigSetOutcome::InvalidValue { ref value, .. } if value == "gemini")
        );

        // The prior value must survive the rejection.
        let reloaded = provider.load().unwrap();
        assert_eq!(
            reloaded.defaults.global_assistant_harness.as_deref(),
            Some("claude")
        );
    }

    /// Pins that unknown keys are surfaced rather than silently
    /// accepted.
    #[test]
    fn config_set_rejects_unknown_config_key() {
        let provider = InMemoryConfigProvider::new();
        let args = argv(&["workbridge", "config", "set", "bogus-key", "value"]);
        let outcome = apply_config_set(&provider, &args);
        assert_eq!(outcome, ConfigSetOutcome::UnknownKey("bogus-key".into()));
    }

    /// Pins the missing-args branch: both key and value are required.
    #[test]
    fn config_set_missing_args_returns_error() {
        let provider = InMemoryConfigProvider::new();
        let args = argv(&["workbridge", "config", "set"]);
        assert_eq!(
            apply_config_set(&provider, &args),
            ConfigSetOutcome::MissingArgs
        );
        let args = argv(&["workbridge", "config", "set", "global-assistant-harness"]);
        assert_eq!(
            apply_config_set(&provider, &args),
            ConfigSetOutcome::MissingArgs
        );
    }
}
