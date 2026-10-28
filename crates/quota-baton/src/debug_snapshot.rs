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
    #[command(about = "Execute this router on a sealed decision-tape epoch")]
    TapeRun {
        #[arg(long)]
        epoch: PathBuf,
        #[arg(long)]
        out: PathBuf,
        #[arg(long, hide = true)]
        plant: Option<u64>,
    },
    #[command(about = "Check decision identity, planted power and margin-zero move totals")]
    TapeCompare {
        #[arg(long)]
        epoch: PathBuf,
        #[arg(long)]
        reference: PathBuf,
        #[arg(long)]
        candidate: PathBuf,
        #[arg(long)]
        power: PathBuf,
        #[arg(long)]
        reference_build: String,
        #[arg(long)]
        candidate_build: String,
        #[arg(long)]
        out: PathBuf,
    },
}

pub async fn run(home: &Path, command: DebugCommand) -> anyhow::Result<()> {
    match command {
        DebugCommand::Snapshot { out } => {
            let bytes = crate::platform::capture_debug_snapshot(home).await?;
            write_new(&out, &bytes)
        }
        DebugCommand::TapeRun { epoch, out, plant } => {
            let result = std::thread::Builder::new()
                .name("qb-decision-tape-reader".into())
                .spawn(move || -> anyhow::Result<_> {
                    let capture = qb_core::observability::decision_reader::load(&epoch)?;
                    Ok(qb_core::observability::decision_reader::execute(
                        &capture,
                        executable_digest()?,
                        plant,
                    )?)
                })?
                .join()
                .map_err(|_| anyhow::anyhow!("decision reader thread failed"))?;
            write_verdict(&out, result)
        }
        DebugCommand::TapeCompare {
            epoch,
            reference,
            candidate,
            power,
            reference_build,
            candidate_build,
            out,
        } => {
            let result = (|| -> anyhow::Result<_> {
                use qb_core::observability::decision_reader;
                let capture = decision_reader::load(&epoch)?;
                decision_reader::compare(
                    &capture,
                    &read_execution(&reference)?,
                    &read_execution(&candidate)?,
                    &read_execution(&power)?,
                    &reference_build,
                    &candidate_build,
                )
                .map_err(Into::into)
            })();
            let passed = result.as_ref().is_ok_and(|report| {
                report.paired.verdict == qb_core::observability::decision_comparison::Verdict::Pass
            });
            write_verdict(&out, result)?;
            anyhow::ensure!(passed, "decision tape verdict did not pass");
            Ok(())
        }
    }
}

fn executable_digest() -> anyhow::Result<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut file = std::fs::File::open(std::env::current_exe()?)?;
    let mut digest = Sha256::new();
    let mut bytes = [0u8; 65536];
    loop {
        let count = file.read(&mut bytes)?;
        if count == 0 {
            break;
        }
        digest.update(&bytes[..count]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn read_execution(
    path: &Path,
) -> anyhow::Result<qb_core::observability::decision_reader::Execution> {
    use std::io::Read;
    let file = std::fs::File::open(path)?;
    anyhow::ensure!(
        file.metadata()?.is_file() && file.metadata()?.len() <= 128 * 1024 * 1024,
        "decision execution exceeds bound"
    );
    let mut bytes = Vec::new();
    file.take(128 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
    anyhow::ensure!(
        bytes.len() <= 128 * 1024 * 1024,
        "decision execution exceeds bound"
    );
    Ok(serde_json::from_slice(&bytes)?)
}

fn write_verdict(out: &Path, result: anyhow::Result<impl serde::Serialize>) -> anyhow::Result<()> {
    match result {
        Ok(report) => write_new(out, &serde_json::to_vec_pretty(&report)?),
        Err(_) => {
            write_new(out, br#"{"verdict":"insufficient","reason":"decision_tape_input_or_execution_refused"}"#)?;
            anyhow::bail!("decision tape input or execution refused; insufficient verdict written")
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

    #[tokio::test]
    async fn missing_tape_writes_insufficient_without_starting_a_home() {
        let directory = tempfile::tempdir().unwrap();
        let home = directory.path().join("unused-home");
        let out = directory.path().join("verdict.json");
        let result = run(
            &home,
            DebugCommand::TapeRun {
                epoch: directory.path().join("missing-epoch"),
                out: out.clone(),
                plant: None,
            },
        )
        .await;
        assert!(result.is_err());
        let value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(out).unwrap()).unwrap();
        assert_eq!(value["verdict"], "insufficient");
        assert!(value.get("candidate_moves").is_none());
        assert!(!home.exists());
    }
}
