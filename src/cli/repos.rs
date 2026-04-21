//! `workbridge repos` subcommand handlers.

use super::{load_config_or_exit, print_repo_list, save_config_or_exit};

pub fn handle_repos_subcommand(args: &[String]) {
    match args.get(2).map(std::string::String::as_str) {
        Some("add") => {
            let Some(path) = args.get(3) else {
                eprintln!("Usage: workbridge repos add <path>");
                std::process::exit(1);
            };
            let mut cfg = load_config_or_exit();
            match cfg.add_repo(path) {
                Ok(display) => {
                    save_config_or_exit(&cfg);
                    println!("Added repo: {display}");
                }
                Err(e) => {
                    eprintln!("Error: {e}");
                    std::process::exit(1);
                }
            }
        }
        Some("add-base") => {
            let Some(path) = args.get(3) else {
                eprintln!("Usage: workbridge repos add-base <path>");
                std::process::exit(1);
            };
            let mut cfg = load_config_or_exit();
            match cfg.add_base_dir(path) {
                Ok((display, count)) => {
                    save_config_or_exit(&cfg);
                    println!("Added base directory: {display} ({count} repos discovered)");
                }
                Err(e) => {
                    eprintln!("Error: {e}");
                    std::process::exit(1);
                }
            }
        }
        Some("remove") => {
            let Some(path) = args.get(3) else {
                eprintln!("Usage: workbridge repos remove <path>");
                std::process::exit(1);
            };
            let mut cfg = load_config_or_exit();
            if cfg.remove_path(path) {
                save_config_or_exit(&cfg);
                println!("Removed: {path}");
            } else {
                println!("Path not found in config: {path}");
            }
        }
        Some("list") => {
            let show_all = args.get(3).is_some_and(|a| a == "--all");
            print_repo_list(&load_config_or_exit(), show_all);
        }
        None => {
            print_repo_list(&load_config_or_exit(), false);
        }
        Some(unknown) => {
            eprintln!("Unknown repos subcommand: {unknown}");
            eprintln!("Usage: workbridge repos [list|add|add-base|remove]");
            std::process::exit(1);
        }
    }
}
