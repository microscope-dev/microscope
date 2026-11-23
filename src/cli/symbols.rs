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
    BuildId, extract_build_id, has_debug_or_symtab, has_dwarf_sections, strip_to_debug_elf,
};
use ring::digest::{SHA256, digest};
use serde::{Deserialize, Serialize};

use super::auth::http_agent;

const CONTROL_TIMEOUT: Duration = Duration::from_secs(30);
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(300);
const MAX_CHECK_BATCH: usize = 128;
const MAX_BATCH_PAYLOAD_BYTES: usize = 128 * 1024 * 1024;

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

    /// Re-upload and replace symbols even if the server already has this build ID.
    #[arg(long)]
    pub force: bool,

    /// Extra PEM root certificates for the server.
    #[arg(long, value_name = "FILE")]
    pub ca_file: Option<PathBuf>,
}

struct DiscoveredPath {
    path: PathBuf,
    build_id: BuildId,
    has_dwarf: bool,
}

struct PreparedSymbol {
    path: PathBuf,
    build_id: BuildId,
    has_dwarf: bool,
    payload: Vec<u8>,
}

#[derive(Serialize)]
struct CheckCandidate {
    build_id: String,
    byte_len: u64,
    has_dwarf: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    replace: bool,
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
    #[serde(default, alias = "upload_headers", rename = "uploadHeaders")]
    headers: BTreeMap<String, String>,
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

fn resolve_upload_target_with(
    server_arg: Option<&str>,
    token_arg: Option<&str>,
    env_server: Option<&str>,
    env_symbol_token: Option<&str>,
    env_token: Option<&str>,
    saved_creds: Option<&credentials::Credentials>,
) -> Result<(Server, String)> {
    let server_raw = server_arg
        .filter(|v| !v.trim().is_empty())
        .or_else(|| env_server.filter(|v| !v.trim().is_empty()))
        .or_else(|| saved_creds.map(|c| c.server.as_str()))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no server URL: pass --server, set MICROSCOPE_SERVER, or run `microscope login`"
            )
        })?;
    let (server, _) = Server::parse(server_raw).map_err(|e| anyhow::anyhow!("{e}"))?;
    let target_origin = credentials::normalize_origin(server_raw);
    let saved_for_origin = saved_creds
        .filter(|c| credentials::normalize_origin(&c.server) == target_origin)
        .map(|c| c.token.as_str());
    let env_server_clean = env_server.filter(|v| !v.trim().is_empty());
    let env_token_for_origin = match env_server_clean {
        Some(env_srv) if credentials::normalize_origin(env_srv) != target_origin => None,
        _ => env_token.filter(|v| !v.trim().is_empty()),
    };

    let token = token_arg
        .filter(|v| !v.trim().is_empty())
        .or_else(|| env_symbol_token.filter(|v| !v.trim().is_empty()))
        .or(env_token_for_origin)
        .or(saved_for_origin)
        .ok_or_else(|| {
            if env_token.is_some_and(|v| !v.trim().is_empty())
                && env_token_for_origin.is_none()
            {
                anyhow::anyhow!(
                    "MICROSCOPE_TOKEN is paired with a different MICROSCOPE_SERVER; pass --token or set MICROSCOPE_SYMBOL_TOKEN for {server_raw}"
                )
            } else if saved_creds.is_some() && saved_for_origin.is_none() {
                anyhow::anyhow!(
                    "saved login credentials belong to a different server; pass --token or set MICROSCOPE_SYMBOL_TOKEN for {server_raw}"
                )
            } else {
                anyhow::anyhow!(
                    "no upload token: pass --token, set MICROSCOPE_SYMBOL_TOKEN, or run `microscope login`"
                )
            }
        })?;
    Ok((server, token.to_owned()))
}

fn resolve_upload_target(args: &UploadSymbolsArgs) -> Result<(Server, String)> {
    let saved_creds = credentials::load();
    let env_server = std::env::var(credentials::SERVER_ENV).ok();
    let env_symbol_token = std::env::var("MICROSCOPE_SYMBOL_TOKEN").ok();
    let env_token = std::env::var(credentials::TOKEN_ENV).ok();
    resolve_upload_target_with(
        args.server.as_deref(),
        args.token.as_deref(),
        env_server.as_deref(),
        env_symbol_token.as_deref(),
        env_token.as_deref(),
        saved_creds.as_ref(),
    )
}

struct UploadContext<'a> {
    control_agent: &'a ureq::Agent,
    ca_file: Option<&'a Path>,
    server: &'a Server,
    auth_header: &'a str,
    project: Option<&'a str>,
    service: Option<&'a str>,
    git_commit: Option<&'a str>,
    force: bool,
}

fn upload_one_missing(
    ctx: &UploadContext<'_>,
    item: &MissingUpload,
    sym: &PreparedSymbol,
) -> Result<CommitResponse> {
    let put_url = if item.upload_url.starts_with('/') {
        ctx.server.url(&item.upload_url)
    } else {
        item.upload_url.clone()
    };

    let upload_agent =
        super::auth::http_agent_for_url(UPLOAD_TIMEOUT, ctx.ca_file, &put_url, ctx.server)?;
    let has_content_type = item
        .headers
        .keys()
        .any(|k| k.eq_ignore_ascii_case("content-type"));
    let mut put_req = upload_agent.put(&put_url);
    if !has_content_type {
        put_req = put_req.header("content-type", "application/octet-stream");
    }
    for (k, v) in &item.headers {
        put_req = put_req.header(k.as_str(), v.as_str());
    }
    let mut put_resp = put_req
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

fn flush_upload_batch(
    ctx: &UploadContext<'_>,
    batch: &mut BTreeMap<String, PreparedSymbol>,
    uploaded: &mut usize,
    skipped: &mut usize,
) -> Result<()> {
    if batch.is_empty() {
        return Ok(());
    }
    let check_req = CheckRequest {
        project: ctx.project.map(str::to_owned),
        candidates: batch
            .values()
            .map(|item| CheckCandidate {
                build_id: item.build_id.as_str().to_owned(),
                byte_len: item.payload.len() as u64,
                has_dwarf: item.has_dwarf,
                replace: ctx.force,
                service_name: ctx.service.map(str::to_owned),
                git_commit: ctx.git_commit.map(str::to_owned),
            })
            .collect(),
    };
    let check_body = serde_json::to_vec(&check_req)?;
    let mut check_resp = ctx
        .control_agent
        .post(ctx.server.url("/api/symbols/v1/check"))
        .header("content-type", "application/json")
        .header("authorization", ctx.auth_header)
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

    *skipped += checked.present.len();
    for item in checked.missing {
        let Some(sym) = batch.get(&item.build_id) else {
            continue;
        };
        let committed = upload_one_missing(ctx, &item, sym)?;
        println!(
            "uploaded {} ({}, {} bytes, {} symbols)",
            committed.build_id,
            sym.path.display(),
            committed.byte_len,
            committed.symbol_count
        );
        *uploaded += 1;
    }
    batch.clear();
    Ok(())
}

fn upload(args: &UploadSymbolsArgs) -> Result<()> {
    let (server, token) = resolve_upload_target(args)?;

    let git_commit = if args.no_git_commit {
        None
    } else {
        args.git_commit.clone().or_else(detect_git_commit)
    };

    let discovered = discover_symbol_paths(&args.paths)?;
    if discovered.is_empty() {
        bail!("no ELF files with GNU build IDs and debug/symbol tables found");
    }

    let control_agent = http_agent(CONTROL_TIMEOUT, args.ca_file.as_deref())?;
    let auth_header = format!("Bearer {}", token.trim());

    let ctx = UploadContext {
        control_agent: &control_agent,
        ca_file: args.ca_file.as_deref(),
        server: &server,
        auth_header: &auth_header,
        project: args.project.as_deref(),
        service: args.service.as_deref(),
        git_commit: git_commit.as_deref(),
        force: args.force,
    };

    let mut uploaded = 0_usize;
    let mut skipped = 0_usize;
    let mut batch: BTreeMap<String, PreparedSymbol> = BTreeMap::new();
    let mut batch_bytes = 0_usize;
    let strip = !args.no_strip;

    for item in discovered.into_values() {
        let prepared = prepare_discovered_symbol(&item, strip)?;
        let next_bytes = prepared.payload.len();
        if !batch.is_empty()
            && (batch.len() >= MAX_CHECK_BATCH
                || batch_bytes.saturating_add(next_bytes) > MAX_BATCH_PAYLOAD_BYTES)
        {
            flush_upload_batch(&ctx, &mut batch, &mut uploaded, &mut skipped)?;
            batch_bytes = 0;
        }
        batch_bytes = batch_bytes.saturating_add(next_bytes);
        batch.insert(prepared.build_id.as_str().to_owned(), prepared);
    }
    flush_upload_batch(&ctx, &mut batch, &mut uploaded, &mut skipped)?;

    if skipped > 0 {
        println!("skipped {skipped} already present on server");
    }
    println!("done: uploaded {uploaded}, skipped {skipped}");
    Ok(())
}

fn prepare_discovered_symbol(item: &DiscoveredPath, strip: bool) -> Result<PreparedSymbol> {
    let raw = fs::read(&item.path)
        .with_context(|| format!("failed to read {} during upload", item.path.display()))?;
    let build_id = extract_build_id(&raw).with_context(|| {
        format!(
            "{} no longer contains a valid GNU build ID",
            item.path.display()
        )
    })?;
    if build_id != item.build_id {
        bail!(
            "{} changed on disk (build ID {} != discovered {})",
            item.path.display(),
            build_id,
            item.build_id
        );
    }
    if !has_debug_or_symtab(&raw) {
        bail!(
            "{} no longer contains debug or symbol sections",
            item.path.display()
        );
    }
    let has_dwarf = has_dwarf_sections(&raw);
    let payload = if strip {
        strip_to_debug_elf(&raw).with_context(|| {
            format!(
                "failed to strip executable sections from {}; pass --no-strip to upload the unstripped binary",
                item.path.display()
            )
        })?
    } else {
        raw
    };
    Ok(PreparedSymbol {
        path: item.path.clone(),
        build_id,
        has_dwarf,
        payload,
    })
}

fn discover_symbol_paths(paths: &[PathBuf]) -> Result<BTreeMap<String, DiscoveredPath>> {
    let mut files = Vec::new();
    for path in paths {
        collect_files(path, &mut files)?;
    }

    let mut by_build_id: BTreeMap<String, DiscoveredPath> = BTreeMap::new();
    for path in files {
        let Ok(Some(build_id)) = microscope_data::symbols::read_elf_build_id_from_file(&path)
        else {
            continue;
        };
        let Ok(raw) = fs::read(&path) else {
            continue;
        };
        if !has_debug_or_symtab(&raw) {
            continue;
        }
        let has_dwarf = has_dwarf_sections(&raw);
        let key = build_id.as_str().to_owned();
        let replace = by_build_id
            .get(&key)
            .is_none_or(|prev| has_dwarf && !prev.has_dwarf);
        if !replace {
            continue;
        }
        by_build_id.insert(
            key,
            DiscoveredPath {
                path,
                build_id,
                has_dwarf,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn implicit_saved_token_is_not_sent_to_different_server() {
        let saved = credentials::Credentials {
            server: "https://app.microscope.dev".to_owned(),
            token: "msu_saved_personal_token".to_owned(),
            username: Some("alice".to_owned()),
            email: None,
        };

        let (_, token) = resolve_upload_target_with(
            Some("https://APP.MICROSCOPE.DEV/"),
            None,
            None,
            None,
            None,
            Some(&saved),
        )
        .unwrap();
        assert_eq!(token, "msu_saved_personal_token");

        let err = resolve_upload_target_with(
            Some("https://other.example.com"),
            None,
            None,
            None,
            None,
            Some(&saved),
        )
        .unwrap_err();
        assert!(err.to_string().contains("different server"), "{err:#}");

        let (_, explicit) = resolve_upload_target_with(
            Some("https://other.example.com"),
            Some("msp_explicit"),
            None,
            None,
            None,
            Some(&saved),
        )
        .unwrap();
        assert_eq!(explicit, "msp_explicit");

        let env_err = resolve_upload_target_with(
            Some("https://other.example.com"),
            None,
            Some("https://app.microscope.dev"),
            None,
            Some("msu_env_paired_token"),
            None,
        )
        .unwrap_err();
        assert!(
            env_err.to_string().contains("MICROSCOPE_TOKEN"),
            "{env_err:#}"
        );

        let (_, env_same) = resolve_upload_target_with(
            Some("https://app.microscope.dev/"),
            None,
            Some("https://app.microscope.dev"),
            None,
            Some("msu_env_paired_token"),
            None,
        )
        .unwrap();
        assert_eq!(env_same, "msu_env_paired_token");
    }

    #[test]
    fn prepare_discovered_symbol_fails_when_file_removed_or_changed() {
        let dir =
            std::env::temp_dir().join(format!("microscope-cli-sym-prep-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let missing = dir.join("gone.elf");
        let bid = BuildId::parse("deadbeefdeadbeefdeadbeefdeadbeefdeadbeef").unwrap();
        let err = prepare_discovered_symbol(
            &DiscoveredPath {
                path: missing,
                build_id: bid.clone(),
                has_dwarf: true,
            },
            true,
        )
        .err()
        .expect("removed file must return an error");
        assert!(err.to_string().contains("failed to read"), "{err:#}");

        let changed = dir.join("changed.elf");
        std::fs::write(&changed, b"not an elf").unwrap();
        let err2 = prepare_discovered_symbol(
            &DiscoveredPath {
                path: changed,
                build_id: bid,
                has_dwarf: true,
            },
            true,
        )
        .err()
        .expect("modified non-ELF file must return an error");
        assert!(err2.to_string().contains("GNU build ID"), "{err2:#}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
