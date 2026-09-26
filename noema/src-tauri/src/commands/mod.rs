//! Tauri command modules.
//!
//! With the admin UI as the shared frontend, noema is now a thin shell:
//! all daemon calls go through HTTP+WS like the web build, so the only
//! Tauri commands left are bootstrapping (daemon URL discovery) and
//! deep-link OAuth forwarding. Native-only capabilities (CPAL audio,
//! file dialogs, Whisper) will land here later via a NativeBridge.

pub mod init;

