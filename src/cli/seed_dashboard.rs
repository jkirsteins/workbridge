//! `workbridge seed-dashboard` subcommand.

use crate::dashboard_seed;

/// Dev tool: populate a workbridge `work-items/` directory with synthetic
/// data so the metrics Dashboard can be visually verified end-to-end.
/// Intended to be run against an isolated `HOME` override (see
/// `docs/metrics.md` for the recommended tmux harness flow).
pub fn handle_seed_dashboard_subcommand(args: &[String]) {
    let Some(dir) = args.get(2) else {
        eprintln!("Usage: workbridge seed-dashboard <work-items-dir>");
        std::process::exit(1);
    };
    if let Err(e) = dashboard_seed::seed_dashboard(std::path::Path::new(dir)) {
        eprintln!("workbridge: seed-dashboard failed: {e}");
        std::process::exit(1);
    }
}
