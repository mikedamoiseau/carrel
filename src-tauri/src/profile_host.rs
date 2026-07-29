//! Bridge between the web layer's [`ProfileHost`] and the Tauri app.
//!
//! The web server deliberately depends on neither Tauri nor `AppState` (see
//! `web_server::ProfileHost`), but remote profile switching needs both: the
//! pool map and `profile_lifecycle` lock live on `AppState`, and the
//! plugin-host rebuild needs an `AppHandle`. This module is the only place the
//! two meet — it hands the web layer one object built from an `AppHandle`,
//! which is `Clone + Send + Sync`.
//!
//! Both operations delegate to the same functions the desktop `switch_profile`
//! / `get_profiles` commands use, so a remote switch is the identical
//! validated sequence and cannot drift from the desktop's rules.

use crate::commands::AppState;
use crate::error::FolioResult;
use crate::web_server::{ProfileHost, WebProfile};
use tauri::{AppHandle, Manager};

/// [`ProfileHost`] backed by a live Tauri app.
struct TauriProfileHost {
    app: AppHandle,
}

impl ProfileHost for TauriProfileHost {
    fn list(&self) -> FolioResult<Vec<WebProfile>> {
        crate::commands::list_profiles_with_lock_state(&self.app.state::<AppState>())
    }

    fn switch(
        &self,
        name: String,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = FolioResult<()>> + Send + '_>> {
        Box::pin(async move {
            let state = self.app.state::<AppState>();
            crate::commands::switch_active_profile(&self.app, &state, name).await
        })
    }
}

/// The profile host to put on `WebState` for a running app.
pub fn for_app(app: &AppHandle) -> std::sync::Arc<dyn ProfileHost> {
    std::sync::Arc::new(TauriProfileHost { app: app.clone() })
}
