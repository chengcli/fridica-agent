use anyhow::{anyhow, Result};
use std::process::Command;

pub(crate) fn command(name: &str, package_arg: &str) -> Result<Command> {
    let executable = if cfg!(windows) {
        format!("{name}.cmd")
    } else {
        name.to_owned()
    };
    let installed = std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|dir| dir.join(&executable))
            .find(|file| file.is_file())
    });
    let command = if let Some(path) = installed {
        Command::new(path)
    } else {
        let mut npm = Command::new(if cfg!(windows) { "npm.cmd" } else { "npm" });
        // npm exec also consults the current project's dependencies and .npmrc.
        // Resolve the server in the worker user's environment.
        let home =
            std::env::home_dir().ok_or_else(|| anyhow!("cannot locate worker home for npm"))?;
        npm.current_dir(home).args([
            "exec",
            "--yes",
            "--ignore-scripts",
            "--prefer-offline",
            package_arg,
            "--",
            name,
        ]);
        npm
    };
    Ok(command)
}

pub(crate) fn run(mut command: Command) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        Err(command.exec().into())
    }
    #[cfg(not(unix))]
    {
        anyhow::ensure!(
            command.status()?.success(),
            "MCP server exited unsuccessfully"
        );
        Ok(())
    }
}
