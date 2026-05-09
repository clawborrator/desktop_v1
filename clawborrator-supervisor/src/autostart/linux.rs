// Linux autostart — stub. Two reasonable real implementations:
//   1. systemd --user service (preferred on systemd distros).
//   2. ~/.config/autostart/clawborrator-supervisor.desktop (XDG;
//      works on KDE/GNOME without systemd assumptions).
// Deferred until we have a Linux user who needs it.

use std::path::Path;

use anyhow::{bail, Result};

use super::{AutostartProvider, AutostartStatus};

pub struct LinuxAutostart;

impl AutostartProvider for LinuxAutostart {
    fn install(&self, _exe: &Path) -> Result<()> {
        bail!("Linux autostart (systemd-user / XDG autostart) is not yet implemented")
    }

    fn uninstall(&self) -> Result<()> {
        bail!("Linux autostart (systemd-user / XDG autostart) is not yet implemented")
    }

    fn status(&self) -> Result<AutostartStatus> {
        Ok(AutostartStatus::NotInstalled)
    }

    fn facility_name(&self) -> &'static str { "systemd user service" }
}
