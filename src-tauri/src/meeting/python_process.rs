//! Background process policy shared by every local Python worker.

use std::path::Path;
use std::process::{Command, Stdio};

/// Keep Python workers non-interactive while preserving their progress/log pipes.
pub(super) fn python_command(executable: &Path) -> Command {
    let mut command = Command::new(executable);
    command.stdin(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;

        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}

#[cfg(test)]
mod tests;
