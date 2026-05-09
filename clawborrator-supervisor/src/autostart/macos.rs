// macOS autostart — stub. Real implementation will write a launchd
// LaunchAgent plist to ~/Library/LaunchAgents/com.clawborrator.
// supervisor.plist and `launchctl load` it. Deferred until a Mac user
// actually needs it.

use std::path::Path;

use anyhow::{bail, Result};

use super::{AutostartProvider, AutostartStatus};

pub struct MacosAutostart;

impl AutostartProvider for MacosAutostart {
    fn install(&self, _exe: &Path) -> Result<()> {
        bail!("macOS autostart (launchd LaunchAgent) is not yet implemented")
    }

    fn uninstall(&self) -> Result<()> {
        bail!("macOS autostart (launchd LaunchAgent) is not yet implemented")
    }

    fn status(&self) -> Result<AutostartStatus> {
        Ok(AutostartStatus::NotInstalled)
    }

    fn facility_name(&self) -> &'static str { "launchd LaunchAgent" }
}
