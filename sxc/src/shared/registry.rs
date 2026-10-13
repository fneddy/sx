use std::{path::PathBuf, sync::Arc};

use sx_core::{HandlerRegistry, PluginStore};

/// Build the handler registry.
///
/// If `plugin_dirs` is provided (non-empty), only those directories are scanned
/// and the default user plugin directory is disabled.
/// If `plugin_dirs` is empty, the default user plugin directory (`~/.config/sx/plugins`)
/// is scanned.
///
/// This is the single canonical implementation shared by both `sxc` and `sxp`.
pub fn build_registry(plugin_dirs: &[PathBuf]) -> Arc<HandlerRegistry> {
    let mut registry = HandlerRegistry::new();
    let mut store = PluginStore::new();

    if plugin_dirs.is_empty() {
        if let Some(config_dir) = dirs_next::config_dir() {
            let plugin_dir = config_dir.join("sx").join("plugins");
            log::debug!("registry: scanning default user plugin dir {:?}", plugin_dir);
            store.load_dir(&plugin_dir);
        } else {
            log::debug!("registry: no config dir found, skipping user plugin dir");
        }
    } else {
        log::debug!(
            "registry: building with {} custom plugin dir(s) (default plugin dir disabled)",
            plugin_dirs.len()
        );
        for dir in plugin_dirs {
            log::info!("registry: scanning plugin dir {:?}", dir);
            store.load_dir(dir);
        }
    }

    store.register_all(&mut registry);

    log::debug!("registry: build complete");
    Arc::new(registry)
}
