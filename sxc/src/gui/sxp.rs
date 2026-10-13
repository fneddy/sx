//! `sxp` — GUI frontend binary using GPUI. Requires feature `gpui`.
//!
//! Parses only `--path` and `--plugin` from the CLI; the query string is
//! entered interactively in the search box. Hands off to [`sxc::gui::run`].

use std::path::PathBuf;

use clap::Parser;

#[derive(Parser)]
#[command(
    name = "sxp",
    about = "Universal query dispatch — file search, calc, sys, and more (GUI GPUI)",
    long_about = None,
)]
struct Cli {
    /// Additional plugin directories to scan for *.so plugins (may be repeated).
    #[arg(long = "plugin", value_name = "DIR")]
    plugin_dirs: Vec<PathBuf>,

    /// Directory to search (defaults to the config, then the current directory).
    #[arg(long, short = 'd', value_name = "DIR")]
    path: Option<PathBuf>,
}

fn main() {
    env_logger::init();

    let cli = Cli::parse();
    let config = sxc::shared::config::load_or_default();

    // Force GL by disabling all Vulkan ICDs before wgpu touches the loader.
    // This skips the ICD scan + ELF relocation cost (~34 % of GPU init time).
    if config.gpu.force_gl && std::env::var_os("VK_LOADER_DRIVERS_DISABLE").is_none() {
        std::env::set_var("VK_LOADER_DRIVERS_DISABLE", "~all~");
        log::info!("sxp: force_gl — set VK_LOADER_DRIVERS_DISABLE=~all~");
    }

    // GUI uses only the first resolved root (single-directory spotlight model).
    let root = sxc::shared::config::resolve_roots(cli.path, &config)
        .into_iter()
        .next()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

    // Hands off plugin dirs directly to GUI so plugin loading runs concurrently
    // with GPUI window startup rather than blocking before the window opens.
    sxc::gui::run(cli.plugin_dirs, root, config.theme.mode);
}
