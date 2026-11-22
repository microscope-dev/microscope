use std::sync::Arc;

use microscope::agent::wire::Server;
use microscope::credentials;
use serde_json::{Value, json};

use crate::cli::auth::http_agent;
use crate::cli::cloud::{
    CONTROL_TIMEOUT, UploadCaptureOptions, default_server_url, fetch_projects, fetch_whoami,
    poll_device_login, start_device_login, upload_capture,
};

use super::profiles::Profiles;
use super::protocol::{
    CloudLoginPoll, CloudLoginStart, CloudLoginStarted, CloudLoginStatus, CloudProject,
    CloudStatus, ErrorKind, Request, Role, ShareProfile, SharedProfile,
};
use super::session::Failure;

pub const fn is_share_request(request: &Request) -> bool {
    matches!(
        request,
        Request::CloudStatus
            | Request::CloudLoginStart(_)
            | Request::CloudLoginPoll(_)
            | Request::ShareProfile(_)
    )
}

pub async fn handle(
    profiles: &Arc<Profiles>,
    role: Role,
    request: Request,
) -> Result<(Value, Vec<u8>), Failure> {
    if role != Role::Control {
        return Err(Failure::new(
            ErrorKind::Forbidden,
            "this link can only view; ask for a control link to upload captures",
        ));
    }

    let profiles = Arc::clone(profiles);
    tokio::task::spawn_blocking(move || dispatch(&profiles, request))
        .await
        .map_err(|_| Failure::new(ErrorKind::Failed, "cloud request failed"))?
        .map(|val| (val, Vec::new()))
}

fn dispatch(profiles: &Profiles, request: Request) -> Result<Value, Failure> {
    match request {
        Request::CloudStatus => Ok(to_value(&cloud_status())),
        Request::CloudLoginStart(req) => Ok(to_value(&login_start(&req)?)),
        Request::CloudLoginPoll(req) => Ok(to_value(&login_poll(&req)?)),
        Request::ShareProfile(req) => Ok(to_value(&share_profile(profiles, &req)?)),
        _ => Err(Failure::new(
            ErrorKind::Failed,
            "internal: only cloud share requests come here",
        )),
    }
}

fn cloud_status() -> CloudStatus {
    let default_server = default_server_url();
    let Some(creds) = credentials::load() else {
        return CloudStatus {
            logged_in: false,
            server: None,
            default_server,
            username: None,
            email: None,
            projects: Vec::new(),
            error: None,
        };
    };

    let Ok((server, _)) = Server::parse(&creds.server) else {
        return CloudStatus {
            logged_in: false,
            server: Some(creds.server),
            default_server,
            username: None,
            email: None,
            projects: Vec::new(),
            error: Some("stored server URL is invalid".to_owned()),
        };
    };

    let Ok(agent) = http_agent(CONTROL_TIMEOUT, None) else {
        return CloudStatus {
            logged_in: false,
            server: Some(server.origin().to_owned()),
            default_server,
            username: creds.username,
            email: creds.email,
            projects: Vec::new(),
            error: Some("could not initialize HTTP client".to_owned()),
        };
    };

    let whoami = match fetch_whoami(&agent, &server, &creds.token) {
        Ok(me) => me,
        Err(error) => {
            return CloudStatus {
                logged_in: false,
                server: Some(server.origin().to_owned()),
                default_server,
                username: creds.username,
                email: creds.email,
                projects: Vec::new(),
                error: Some(error.to_string()),
            };
        }
    };

    let projects = match fetch_projects(&agent, &server, &creds.token) {
        Ok(list) => list
            .into_iter()
            .filter(|p| !p.is_unassigned && matches!(p.permission.as_str(), "write" | "admin"))
            .map(|p| CloudProject {
                id: p.id,
                name: p.name,
                org_id: p.org.as_ref().map_or(0, |o| o.id),
                org_name: p.org.map(|o| o.name),
            })
            .collect(),
        Err(error) => {
            return CloudStatus {
                logged_in: true,
                server: Some(server.origin().to_owned()),
                default_server,
                username: Some(whoami.username),
                email: Some(whoami.email),
                projects: Vec::new(),
                error: Some(error.to_string()),
            };
        }
    };

    CloudStatus {
        logged_in: true,
        server: Some(server.origin().to_owned()),
        default_server,
        username: Some(whoami.username),
        email: Some(whoami.email),
        projects,
        error: None,
    }
}

fn login_start(req: &CloudLoginStart) -> Result<CloudLoginStarted, Failure> {
    let raw = req
        .server
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map_or_else(default_server_url, str::to_owned);
    let (server, _) =
        Server::parse(&raw).map_err(|error| Failure::new(ErrorKind::Invalid, error.to_string()))?;
    let agent = http_agent(CONTROL_TIMEOUT, None)
        .map_err(|error| Failure::new(ErrorKind::Failed, error.to_string()))?;
    let started = start_device_login(&agent, &server)
        .map_err(|error| Failure::new(ErrorKind::Failed, error.to_string()))?;

    Ok(CloudLoginStarted {
        server: server.origin().to_owned(),
        verify_url: started.verify_url,
        user_code: started.user_code,
        poll_token: started.poll_token,
        expires_in_secs: started.expires_in_secs,
        poll_interval_ms: started.poll_interval_ms,
    })
}

fn login_poll(req: &CloudLoginPoll) -> Result<CloudLoginStatus, Failure> {
    let (server, _) = Server::parse(&req.server)
        .map_err(|error| Failure::new(ErrorKind::Invalid, error.to_string()))?;
    let agent = http_agent(CONTROL_TIMEOUT, None)
        .map_err(|error| Failure::new(ErrorKind::Failed, error.to_string()))?;
    let polled = poll_device_login(&agent, &server, &req.poll_token)
        .map_err(|error| Failure::new(ErrorKind::Failed, error.to_string()))?;

    Ok(CloudLoginStatus {
        status: polled.status,
        username: polled.username,
        email: polled.email,
    })
}

fn share_profile(profiles: &Profiles, req: &ShareProfile) -> Result<SharedProfile, Failure> {
    let creds = credentials::load().ok_or_else(|| {
        Failure::new(
            ErrorKind::Invalid,
            "not signed in; sign in or run `microscope login` first",
        )
    })?;
    let (server, _) = Server::parse(&creds.server)
        .map_err(|error| Failure::new(ErrorKind::Invalid, error.to_string()))?;

    let (default_name, profile) = profiles.snapshot(&req.profile)?;
    let name = req
        .name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map_or(default_name, str::to_owned);

    let uploaded = upload_capture(
        &server,
        &creds.token,
        None,
        profile.capture().clone(),
        &UploadCaptureOptions {
            project_id: Some(req.project_id),
            project: None,
            org_id: None,
            name,
        },
    )
    .map_err(|error| Failure::new(ErrorKind::Failed, error.to_string()))?;

    Ok(SharedProfile {
        url: uploaded.url,
        session_id: uploaded.session_id,
        capture_id: uploaded.capture_id,
        project_name: uploaded.project_name,
        byte_len: uploaded.byte_len,
        redacted_count: uploaded.redacted_count,
    })
}

fn to_value(value: &impl serde::Serialize) -> Value {
    serde_json::to_value(value).unwrap_or_else(|error| json!({ "error": error.to_string() }))
}
