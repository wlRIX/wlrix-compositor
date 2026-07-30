// SPDX-License-Identifier: GPL-3.0-or-later
//! `wp_security_context_v1`: letting a sandbox tell the compositor which clients are sandboxed.
//!
//! A sandbox engine (Flatpak's `bwrap`, say) asks for a *restricted socket*, says which engine
//! and app id it is for, and hands that socket to the sandboxed application instead of the
//! session's own. Clients arriving on it are tagged, which is the only reliable way the
//! compositor can tell "the session's own input method" from "some app in a sandbox" -- the pid
//! is not trustworthy and the app id is self-reported.
//!
//! That tag is what the privileged protocols filter on: see [`Wlrix::client_is_sandboxed`].
//! Without this, restricting them was impossible, and they had to accept every client.

use smithay::{
    reexports::wayland_server::Client,
    wayland::security_context::{
        SecurityContext, SecurityContextHandler, SecurityContextListenerSource,
    },
};
use std::sync::Arc;
use tracing::info;

use crate::{Wlrix, state::ClientState};

impl Wlrix {
    /// Whether `client` arrived through a sandbox's restricted socket.
    ///
    /// The privileged protocols use this: an ordinary session client may drive the IME or lock
    /// the screen, a sandboxed one may not. A client the compositor knows nothing about is
    /// treated as sandboxed -- the safe way round.
    pub fn client_is_sandboxed(client: &Client) -> bool {
        client
            .get_data::<ClientState>()
            .is_none_or(|data| data.security_context.is_some())
    }
}

impl SecurityContextHandler for Wlrix {
    fn context_created(&mut self, source: SecurityContextListenerSource, context: SecurityContext) {
        info!(
            engine = context.sandbox_engine.as_deref().unwrap_or("<none>"),
            app_id = context.app_id.as_deref().unwrap_or("<none>"),
            "security context created"
        );
        // Clients that connect on this socket carry the context, so anything privileged can
        // recognize them later.
        let inserted = self
            .loop_handle
            .insert_source(source, move |client_stream, _, state| {
                let context = context.clone();
                if let Err(err) = state.display_handle.insert_client(
                    client_stream,
                    Arc::new(ClientState {
                        security_context: Some(context),
                        ..Default::default()
                    }),
                ) {
                    tracing::warn!(?err, "could not add a client from a security context");
                }
            });
        if let Err(err) = inserted {
            tracing::warn!(?err, "could not listen on the security context's socket");
        }
    }
}
