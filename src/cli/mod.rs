//! CLI subcommand handlers.
//!
//! The top-level dispatcher lives in `crate::main`; each submodule
//! implements one subcommand tree (`repos`, `mcp`, `config`,
//! `seed-dashboard`). Shared helpers for loading and saving config and
//! printing the repo list are kept here so every handler can reach them
//! without routing back through `main`.

pub mod config;
pub mod mcp;
pub mod repos;
pub mod seed_dashboard;

use crate::config::{self as cfg_mod, ConfigProvider, FileConfigProvider};

/// Load the on-disk config or exit with an error message.
pub fn load_config_or_exit() -> cfg_mod::Config {
    cfg_mod::Config::load().unwrap_or_else(|e| {
        eprintln!("Error loading config: {e}");
        std::process::exit(1);
    })
}

/// Save the given config or exit with an error message.
pub fn save_config_or_exit(cfg: &cfg_mod::Config) {
    FileConfigProvider.save(cfg).unwrap_or_else(|e| {
        eprintln!("Error saving config: {e}");
        std::process::exit(1);
    });
}

/// Print the repo list to stdout in the canonical `workbridge repos
/// list` format.
pub fn print_repo_list(cfg: &cfg_mod::Config, show_all: bool) {
    let active = cfg.active_repos();
    let entries = if show_all {
        cfg.all_repos()
    } else {
        active.clone()
    };
    if entries.is_empty() {
        if show_all {
            println!("No repositories configured.");
            println!("Use 'workbridge repos add <path>' to add one.");
        } else {
            println!("No managed repositories.");
            println!("Use 'workbridge repos list --all' to see all,");
            println!("or 'workbridge repos add <path>' to add one.");
        }
    } else {
        let label = if show_all { "ALL" } else { "MANAGED" };
        println!("{label} {:<57} {:<12} AVAILABLE", "PATH", "SOURCE");
        println!("{}", "-".repeat(85));
        let active_paths: Vec<_> = active.iter().map(|e| &e.path).collect();
        for entry in &entries {
            let source = match entry.source {
                cfg_mod::RepoSource::Explicit => "explicit",
                cfg_mod::RepoSource::Discovered => "discovered",
            };
            let avail = if entry.git_dir_present { "yes" } else { "no" };
            let status = if show_all && !active_paths.contains(&&entry.path) {
                " [unmanaged]"
            } else {
                ""
            };
            println!(
                "     {:<57} {:<12} {}{status}",
                entry.path.display(),
                source,
                avail,
            );
        }
        if !show_all {
            let all_count = cfg.all_repos().len();
            let unmanaged = all_count.saturating_sub(active.len());
            if unmanaged > 0 {
                println!("\n{unmanaged} repo(s) available but unmanaged. Use --all to see all.");
            }
        }
    }
}
