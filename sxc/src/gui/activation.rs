//! Activation — executes the action associated with a selected result.
//!
//! This is the only GUI file that performs platform I/O (GIO app launch,
//! `xdg-open`).  Keeping it separate means the display components have no
//! side-effectful code.

use super::result_list::ActionKind;

/// Launch the application or open the file described by `action`.
pub fn activate(action: &ActionKind) {
    match action {
        ActionKind::GioId(id) => {
            use gio::prelude::AppInfoExt;
            match gio::AppInfo::all()
                .into_iter()
                .find(|a| a.id().as_deref() == Some(id.as_ref()))
            {
                Some(app) => {
                    if let Err(e) = app.launch(&[], gio::AppLaunchContext::NONE) {
                        log::warn!("sxp: gio launch {id:?} failed: {e}");
                    }
                }
                None => log::warn!("sxp: gio_id {id:?} not found"),
            }
        }
        ActionKind::FilePath(path) => {
            if let Err(e) = open::that_detached(path.as_ref()) {
                log::warn!("sxp: open {path:?} failed: {e}");
            }
        }
    }
}
