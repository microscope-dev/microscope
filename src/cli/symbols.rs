use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use clap::{Args, Subcommand};
use microscope::agent::wire::Server;
use microscope::credentials;
use microscope_data::symbols::{
    BuildId, extract_build_id, has_debug_or_symtab, strip_to_debug_elf,
};
use ring::digest::{SHA256, digest};
use serde::{Deserialize, Serialize};

use super::auth::http_agent;

const CONTROL_TIMEOUT: Duration = Duration::from_secs(30);
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Args)]
pub struct SymbolsArgs {
    #[command(subcommand)]
    pub command: SymbolsCommand,
}

#[derive(Subcommand)]
pub enum SymbolsCommand {
    /// Upload debug symbols for one or more ELF files or directories.
    Upload(UploadSymbolsArgs),
}

#[derive(Args)]
pub struct UploadSymbolsArgs {
    /// Paths to ELF executables, shared libraries, .debug files, or directories.
    #[arg(required = true, value_name = "PATH")]
    pub paths: Vec<PathBuf>,

    /// Microscope server URL (defaults to `MICROSCOPE_SERVER` or logged-in server).
    #[arg(long, value_name = "URL")]
    pub server: Option<String>,

    /// Project upload token (msp_...) or user token (msu_...).
    #[arg(long, value_name = "TOKEN")]
    pub token: Option<String>,

    /// Project name (required when authenticating with a user token).
    #[arg(long, value_name = "PROJECT")]
    pub project: Option<String>,

    /// Service name to associate with uploaded build IDs.
    #[arg(long, value_name = "SERVICE")]
    pub service: Option<String>,

    /// Git commit SHA to associate with uploaded build IDs.
    #[arg(long, value_name = "SHA")]
    pub git_commit: Option<String>,

    /// Do not auto-detect git commit from the current repository.
    #[arg(long)]
    pub no_git_commit: bool,

    /// Upload full ELF bytes instead of stripping non-debug loadable sections.
    #[arg(long)]
    pub no_strip: bool,

    /// Extra PEM root certificates for the server.
    #[arg(long, value_name = "FILE")]
    pub ca_file: Option<PathBuf>,
}

struct DiscoveredSymbol {
    path: PathBuf,
    build_id: BuildId,
    has_dwarf: bool,
    payload: Vec<u8>,
}

#[derive(Serialize)]
struct CheckCandidate {
    build_id: String,
    byte_len: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    service_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    git_commit: Option<String>,
}

#[derive(Serialize)]
struct CheckRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    project: Option<String>,
    candidates: Vec<CheckCandidate>,
}

#[derive(Deserialize)]
struct MissingUpload {
    build_id: String,
    upload_id: i64,
    upload_url: String,
}

#[derive(Deserialize)]
struct CheckResponse {
    present: Vec<String>,
    missing: Vec<MissingUpload>,
}

#[derive(Serialize)]
struct CommitRequest {
    upload_id: i64,
    build_id: String,
    sha256: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    service_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    git_commit: Option<String>,
}

#[derive(Deserialize)]
struct CommitResponse {
    build_id: String,
    symbol_count: u32,
    byte_len: i64,
}

pub fn run(args: &SymbolsArgs) -> ExitCode {
    let result = match &args.command {
        SymbolsCommand::Upload(upload_args) => upload(upload_args),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn resolve_upload_target(args: &UploadSymbolsArgs) -> Result<(Server, String)> {
    let saved_creds = credentials::load();
    let server_raw = args
        .server
        .clone()
        .or_else(|| {
            std::env::var(credentials::SERVER_ENV)
                .ok()
                .filter(|v| !v.trim().is_empty())
        })
        .or_else(|| saved_creds.as_ref().map(|c| c.server.clone()))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no server URL: pass --server, set MICROSCOPE_SERVER, or run `microscope login`"
            )
        })?;
    let token = args
        .token
        .clone()
        .or_else(|| {
            std::env::var("MICROSCOPE_SYMBOL_TOKEN")
                .ok()
                .filter(|v| !v.trim().is_empty())
        })
        .or_else(|| {
            std::env::var(credentials::TOKEN_ENV)
                .ok()
                .filter(|v| !v.trim().is_empty())
        })
        .or_else(|| saved_creds.map(|c| c.token))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no upload token: pass --token, set MICROSCOPE_SYMBOL_TOKEN, or run `microscope login`"
            )
        })?;
    let (server, _) = Server::parse(&server_raw).map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok((server, token))
}

struct UploadContext<'a> {
    control_agent: &'a ureq::Agent,
    upload_agent: &'a ureq::Agent,
    server: &'a Server,
    auth_header: &'a str,
    service: Option<&'a str>,
    git_commit: Option<&'a str>,
}

fn upload_one_missing(
    ctx: &UploadContext<'_>,
    item: &MissingUpload,
    sym: &DiscoveredSymbol,
) -> Result<CommitResponse> {
    let put_url = if item.upload_url.starts_with('/') {
        ctx.server.url(&item.upload_url)
    } else {
        item.upload_url.clone()
    };

    let mut put_resp = ctx
        .upload_agent
        .put(&put_url)
        .header("content-type", "application/octet-stream")
        .send(sym.payload.as_slice())
        .with_context(|| format!("failed uploading {} ({})", sym.path.display(), sym.build_id))?;
    let put_status = put_resp.status().as_u16();
    if !(200..300).contains(&put_status) {
        let err_body = put_resp
            .body_mut()
            .with_config()
            .limit(64 * 1024)
            .read_to_string()
            .unwrap_or_default();
        bail!(
            "bucket PUT failed for {} ({put_status}): {err_body}",
            sym.build_id
        );
    }

    let sha_hex = sha256_hex(&sym.payload);
    let commit_body = serde_json::to_vec(&CommitRequest {
        upload_id: item.upload_id,
        build_id: item.build_id.clone(),
        sha256: sha_hex,
        service_name: ctx.service.map(str::to_owned),
        git_commit: ctx.git_commit.map(str::to_owned),
    })?;
    let mut commit_resp = ctx
        .control_agent
        .post(ctx.server.url("/api/symbols/v1/commit"))
        .header("content-type", "application/json")
        .header("authorization", ctx.auth_header)
        .send(&commit_body)
        .with_context(|| format!("failed committing {}", sym.build_id))?;
    let commit_status = commit_resp.status().as_u16();
    let commit_text = commit_resp
        .body_mut()
        .with_config()
        .limit(256 * 1024)
        .read_to_string()
        .unwrap_or_default();
    if commit_status != 200 {
        bail!(
            "symbol commit failed for {} ({commit_status}): {commit_text}",
            sym.build_id
        );
    }
    serde_json::from_str(&commit_text).context("unexpected commit response")
}

fn upload(args: &UploadSymbolsArgs) -> Result<()> {
    let (server, token) = resolve_upload_target(args)?;

    let git_commit = if args.no_git_commit {
        None
    } else {
        args.git_commit.clone().or_else(detect_git_commit)
    };

    let candidates = discover_symbols(&args.paths, !args.no_strip)?;
    if candidates.is_empty() {
        bail!("no ELF files with GNU build IDs and debug/symbol tables found");
    }

    let control_agent = http_agent(CONTROL_TIMEOUT, args.ca_file.as_deref())?;
    let upload_agent = http_agent(UPLOAD_TIMEOUT, args.ca_file.as_deref())?;
    let auth_header = format!("Bearer {}", token.trim());

    let check_req = CheckRequest {
        project: args.project.clone(),
        candidates: candidates
            .values()
            .map(|item| CheckCandidate {
                build_id: item.build_id.as_str().to_owned(),
                byte_len: item.payload.len() as u64,
                service_name: args.service.clone(),
                git_commit: git_commit.clone(),
            })
            .collect(),
    };
    let check_body = serde_json::to_vec(&check_req)?;
    let mut check_resp = control_agent
        .post(server.url("/api/symbols/v1/check"))
        .header("content-type", "application/json")
        .header("authorization", &auth_header)
        .send(&check_body)
        .context("could not reach /api/symbols/v1/check")?;
    let status = check_resp.status().as_u16();
    let text = check_resp
        .body_mut()
        .with_config()
        .limit(2 * 1024 * 1024)
        .read_to_string()
        .unwrap_or_default();
    if status != 200 {
        bail!("symbol check failed ({status}): {text}");
    }
    let checked: CheckResponse =
        serde_json::from_str(&text).context("unexpected /api/symbols/v1/check response")?;

    let ctx = UploadContext {
        control_agent: &control_agent,
        upload_agent: &upload_agent,
        server: &server,
        auth_header: &auth_header,
        service: args.service.as_deref(),
        git_commit: git_commit.as_deref(),
    };
    let mut uploaded = 0_usize;
    for item in checked.missing {
        let Some(sym) = candidates.get(&item.build_id) else {
            continue;
        };
        let committed = upload_one_missing(&ctx, &item, sym)?;
        println!(
            "uploaded {} ({}, {} bytes, {} symbols)",
            committed.build_id,
            sym.path.display(),
            committed.byte_len,
            committed.symbol_count
        );
        uploaded += 1;
    }

    if !checked.present.is_empty() {
        println!(
            "skipped {} already present on server",
            checked.present.len()
        );
    }
    println!(
        "done: uploaded {uploaded}, skipped {}",
        checked.present.len()
    );
    Ok(())
}

fn discover_symbols(paths: &[PathBuf], strip: bool) -> Result<BTreeMap<String, DiscoveredSymbol>> {
    let mut files = Vec::new();
    for path in paths {
        collect_files(path, &mut files)?;
    }

    let mut by_build_id: BTreeMap<String, DiscoveredSymbol> = BTreeMap::new();
    for path in files {
        let Ok(raw) = fs::read(&path) else {
            continue;
        };
        let Ok(build_id) = extract_build_id(&raw) else {
            continue;
        };
        if !has_debug_or_symtab(&raw) {
            continue;
        }
        let has_dwarf = object::File::parse(raw.as_slice()).is_ok_and(|obj| {
            use object::Object as _;
            obj.section_by_name(".debug_info").is_some()
                || obj.section_by_name(".zdebug_info").is_some()
        });
        let key = build_id.as_str().to_owned();
        let replace = by_build_id
            .get(&key)
            .is_none_or(|prev| has_dwarf && !prev.has_dwarf);
        if !replace {
            continue;
        }
        let payload = if strip {
            strip_to_debug_elf(&raw).unwrap_or(raw)
        } else {
            raw
        };
        by_build_id.insert(
            key,
            DiscoveredSymbol {
                path,
                build_id,
                has_dwarf,
                payload,
            },
        );
    }

    Ok(by_build_id)
}

fn collect_files(path: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let meta = fs::metadata(path).with_context(|| format!("cannot stat {}", path.display()))?;
    if meta.is_file() {
        out.push(path.to_path_buf());
        return Ok(());
    }
    if meta.is_dir() {
        let mut entries: Vec<PathBuf> = fs::read_dir(path)
            .with_context(|| format!("cannot read directory {}", path.display()))?
            .filter_map(|e| e.ok().map(|entry| entry.path()))
            .collect();
        entries.sort();
        for entry in entries {
            if let Ok(m) = fs::symlink_metadata(&entry) {
                if m.is_file() {
                    out.push(entry);
                } else if m.is_dir() {
                    collect_files(&entry, out)?;
                }
            }
        }
    }
    Ok(())
}

fn detect_git_commit() -> Option<String> {
    let output = Command::new("git")
        .args(["rev-parse", "--short=12", "HEAD"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let sha = String::from_utf8(output.stdout).ok()?;
    let trimmed = sha.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

fn sha256_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let d = digest(&SHA256, bytes);
    let mut out = String::with_capacity(64);
    for b in d.as_ref() {
        let _ = write!(out, "{b:02x}");
    }
    out
}
