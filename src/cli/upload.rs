use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context as _, Result};
use clap::Args;
use microscope::profiler::Profile;

use super::cloud::{UploadCaptureOptions, resolve_target, upload_capture};

#[derive(Args)]
pub struct UploadArgs {
    /// Capture file (.usprof) or segment directory to upload.
    #[arg(value_name = "CAPTURE")]
    pub file: PathBuf,

    /// Target project name or numeric ID.
    #[arg(short = 'p', long, value_name = "PROJECT")]
    pub project: Option<String>,

    /// Target organization ID when a project name matches multiple organizations.
    #[arg(long, value_name = "ORG_ID")]
    pub org: Option<i64>,

    /// Display name for the uploaded capture and session.
    #[arg(long, value_name = "NAME")]
    pub name: Option<String>,

    /// Microscope server URL (defaults to `MICROSCOPE_SERVER` or logged-in server).
    #[arg(long, value_name = "URL")]
    pub server: Option<String>,

    /// Personal API token (msu_...).
    #[arg(long, value_name = "TOKEN")]
    pub token: Option<String>,

    /// Extra PEM root certificates for the server.
    #[arg(long, value_name = "FILE")]
    pub ca_file: Option<PathBuf>,

    /// Print the upload response as JSON.
    #[arg(long)]
    pub json: bool,
}

pub fn run(args: &UploadArgs) -> ExitCode {
    match upload(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn upload(args: &UploadArgs) -> Result<()> {
    let (server, token) = resolve_target(args.server.as_deref(), args.token.as_deref())?;
    let read = Profile::read(&args.file)
        .with_context(|| format!("could not read capture {}", args.file.display()))?;
    if read.truncated {
        eprintln!(
            "warning: {} was cut short; uploading the complete part",
            args.file.display()
        );
    }

    let default_name = args.file.file_name().map_or_else(
        || args.file.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    );
    let name = args
        .name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map_or(default_name, str::to_owned);

    let (project_id, project) = args
        .project
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map_or((None, None), |raw| {
            raw.parse::<i64>()
                .map_or_else(|_| (None, Some(raw.to_owned())), |id| (Some(id), None))
        });

    let result = upload_capture(
        &server,
        &token,
        args.ca_file.as_deref(),
        read.profile.into_capture(),
        &UploadCaptureOptions {
            project_id,
            project,
            org_id: args.org,
            name,
        },
    )?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else {
        let size = format_bytes(result.byte_len);
        if result.redacted_count > 0 {
            println!(
                "Uploaded {} to {} ({size}, {} sensitive {} redacted)",
                result.name,
                result.project_name,
                result.redacted_count,
                if result.redacted_count == 1 {
                    "value"
                } else {
                    "values"
                },
            );
        } else {
            println!(
                "Uploaded {} to {} ({size})",
                result.name, result.project_name
            );
        }
        println!("Session: {}", result.url);
    }

    Ok(())
}

#[expect(
    clippy::cast_precision_loss,
    reason = "human-readable byte formatting needs only f64 precision"
)]
fn format_bytes(bytes: i64) -> String {
    let b = bytes.max(0) as f64;
    if b < 1024.0 {
        format!("{bytes} B")
    } else if b < 1024.0 * 1024.0 {
        format!("{:.1} KiB", b / 1024.0)
    } else {
        format!("{:.1} MiB", b / (1024.0 * 1024.0))
    }
}
