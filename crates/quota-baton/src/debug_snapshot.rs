use std::{
    fs::OpenOptions,
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::Context;
use clap::Subcommand;

#[derive(Subcommand)]
pub enum DebugCommand {
    #[command(about = "Export the running core's display data without credentials")]
    Snapshot {
        #[arg(
            long,
            help = "New JSON output file; an existing file is never overwritten"
        )]
        out: PathBuf,
    },
}

pub async fn run(home: &Path, command: DebugCommand) -> anyhow::Result<()> {
    match command {
        DebugCommand::Snapshot { out } => {
            let bytes = crate::platform::capture_debug_snapshot(home).await?;
            write_new(&out, &bytes)
        }
    }
}

fn write_new(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .context("Unable to create snapshot output file")?;
    if let Err(error) = file.write_all(bytes).and_then(|()| file.sync_all()) {
        drop(file);
        let _ = std::fs::remove_file(path);
        return Err(error).context("Unable to write snapshot output file");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn r16_debug_snapshot_command_requires_output() {
        assert!(crate::Cli::try_parse_from(["quota-baton", "debug", "snapshot"]).is_err());
        let cli = crate::Cli::try_parse_from([
            "quota-baton",
            "debug",
            "snapshot",
            "--out",
            "snapshot.json",
        ])
        .unwrap();
        let crate::Command::Debug {
            command: DebugCommand::Snapshot { out },
        } = cli.command
        else {
            panic!("wrong command");
        };
        assert_eq!(out, PathBuf::from("snapshot.json"));
    }

    #[test]
    fn r16_debug_snapshot_never_overwrites_an_existing_output() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("snapshot.json");
        write_new(&path, b"first").unwrap();
        assert!(write_new(&path, b"second").is_err());
        assert_eq!(std::fs::read(path).unwrap(), b"first");
    }
}
