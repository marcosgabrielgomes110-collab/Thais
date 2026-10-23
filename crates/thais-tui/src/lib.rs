#![forbid(unsafe_code)]

//! TUI client. It speaks only to `thais-sdk` and never imports `thais-core`.

use anyhow::Result;

/// Start the TUI. M0 provides the boundary; the real interface arrives in M1.
pub fn run() -> Result<()> {
    Ok(())
}
