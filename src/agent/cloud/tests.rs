use std::time::{Duration, Instant};

use axum::{
    Router,
    http::{HeaderMap, StatusCode},
    routing::{get, put},
};

use super::{Cloud, Provider, detect, fetch, parse_aws, parse_azure, parse_gcp};
use crate::agent::{host::Root, scratch::Scratch};

const AWS_DOCUMENT: &str = r#"{
  "accountId" : "123456789012", "architecture" : "x86_64",
  "availabilityZone" : "us-west-2b", "imageId" : "ami-0abcdef1234567890",
  "instanceId" : "i-0a1b2c3d4e5f60718", "instanceType" : "m7i.2xlarge",
  "privateIp" : "10.0.3.17", "region" : "us-west-2", "version" : "2017-09-30"
}"#;

const GCP_INSTANCE: &str = r#"{
  "id": 4520035219218447132, "hostname": "vm-1.c.project.internal",
  "machineType": "projects/123456/machineTypes/e2-standard-4",
  "zone": "projects/123456/zones/europe-west4-a",
  "networkInterfaces": [
    {"ip": "10.164.0.2", "accessConfigs": [{"externalIp": "34.90.1.2", "type": "ONE_TO_ONE_NAT"}]},
    {"ip": "10.20.0.5", "accessConfigs": []}
  ]
}"#;

const AZURE_INSTANCE: &str = r#"{
  "compute": {"location": "westeurope", "vmId": "02aab8a4-74ef-476e-8182-f6d2ba4166a6",
              "vmSize": "Standard_D4s_v5", "zone": ""},
  "network": {"interface": [{"ipv4": {"ipAddress": [
    {"privateIpAddress": "10.1.0.4", "publicIpAddress": "20.50.1.2"},
    {"privateIpAddress": "10.1.0.5", "publicIpAddress": ""}
  ]}}]}
}"#;

fn aws() -> Cloud {
    Cloud {
        provider: "aws",
        region: Some("us-west-2".into()),
        zone: Some("us-west-2b".into()),
        instance_type: Some("m7i.2xlarge".into()),
        instance_id: Some("i-0a1b2c3d4e5f60718".into()),
        private_ips: vec!["10.0.3.17".parse().unwrap()],
        public_ips: vec!["203.0.113.24".parse().unwrap()],
    }
}

#[test]
fn each_provider_parses_to_the_same_shape() {
    assert_eq!(
        parse_aws(AWS_DOCUMENT, Some("203.0.113.24\n")).unwrap(),
        aws()
    );
    assert!(parse_aws(AWS_DOCUMENT, None).unwrap().public_ips.is_empty());

    let gcp = parse_gcp(GCP_INSTANCE).unwrap();
    assert_eq!(gcp.instance_id.as_deref(), Some("4520035219218447132"));
    assert_eq!(gcp.zone.as_deref(), Some("europe-west4-a"));
    assert_eq!(gcp.region.as_deref(), Some("europe-west4"));
    assert_eq!(gcp.instance_type.as_deref(), Some("e2-standard-4"));
    assert_eq!(gcp.private_ips.len(), 2);
    assert_eq!(
        gcp.public_ips,
        ["34.90.1.2".parse::<std::net::IpAddr>().unwrap()]
    );

    let azure = parse_azure(AZURE_INSTANCE).unwrap();
    assert_eq!(
        azure.instance_id.as_deref(),
        Some("02aab8a4-74ef-476e-8182-f6d2ba4166a6")
    );
    assert_eq!(azure.region.as_deref(), Some("westeurope"));
    assert_eq!(azure.zone, None);
    assert_eq!(azure.private_ips.len(), 2);
    assert_eq!(azure.public_ips.len(), 1);

    for garbage in [
        "",
        "[]",
        "{}",
        "{\"compute\": {}}",
        "{\"instanceId\": \"\"}",
        "{\"compute\": 1}",
        "<html>",
    ] {
        assert_eq!(parse_aws(garbage, None), None);
        assert_eq!(parse_gcp(garbage), None);
        assert_eq!(parse_azure(garbage), None);
    }
}

#[test]
fn the_provider_comes_from_dmi_so_bare_metal_asks_nobody() {
    let with = |files: &[(&str, &str)]| {
        let scratch = Scratch::new();
        for (name, text) in files {
            let path = scratch.path().join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        detect(&Root::at(scratch.path()))
    };
    assert_eq!(
        with(&[("sys/class/dmi/id/sys_vendor", "Amazon EC2\n")]),
        Some(Provider::Aws)
    );
    assert_eq!(
        with(&[(
            "sys/hypervisor/uuid",
            "EC2E1916-9099-7CAF-FD21-012345ABCDEF\n"
        )]),
        Some(Provider::Aws)
    );
    assert_eq!(
        with(&[("sys/class/dmi/id/product_name", "Google Compute Engine\n")]),
        Some(Provider::Gcp)
    );
    assert_eq!(
        with(&[(
            "sys/class/dmi/id/chassis_asset_tag",
            "7783-7084-3265-9085-8269-3286-77\n"
        )]),
        Some(Provider::Azure)
    );
    assert_eq!(
        with(&[(
            "sys/class/dmi/id/sys_vendor",
            "Micro-Star International Co., Ltd.\n"
        )]),
        None
    );
    assert_eq!(with(&[]), None);
}

fn serve(app: Router) -> (tokio::runtime::Runtime, String) {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let listener = runtime
        .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
        .unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    runtime.spawn(async move { axum::serve(listener, app).await });
    (runtime, base)
}

#[test]
fn aws_is_asked_with_an_imdsv2_token() {
    let app = Router::new()
        .route(
            "/latest/api/token",
            put(|headers: HeaderMap| async move {
                if headers
                    .get("x-aws-ec2-metadata-token-ttl-seconds")
                    .is_some()
                {
                    (StatusCode::OK, "tok")
                } else {
                    (StatusCode::BAD_REQUEST, "")
                }
            }),
        )
        .route(
            "/latest/dynamic/instance-identity/document",
            get(|headers: HeaderMap| async move {
                if headers
                    .get("x-aws-ec2-metadata-token")
                    .is_some_and(|t| t == "tok")
                {
                    (StatusCode::OK, AWS_DOCUMENT)
                } else {
                    (StatusCode::UNAUTHORIZED, "")
                }
            }),
        )
        .route(
            "/latest/meta-data/public-ipv4",
            get(|| async { "203.0.113.24" }),
        );
    let (_runtime, base) = serve(app);
    assert_eq!(
        fetch(Provider::Aws, &base, Duration::from_secs(2)),
        Some(aws())
    );
}

#[test]
fn gcp_and_azure_send_their_headers() {
    let app = Router::new()
        .route(
            "/computeMetadata/v1/instance/",
            get(|headers: HeaderMap| async move {
                if headers
                    .get("metadata-flavor")
                    .is_some_and(|v| v == "Google")
                {
                    (StatusCode::OK, GCP_INSTANCE)
                } else {
                    (StatusCode::FORBIDDEN, "")
                }
            }),
        )
        .route(
            "/metadata/instance",
            get(|headers: HeaderMap| async move {
                if headers.get("metadata").is_some_and(|v| v == "true") {
                    (StatusCode::OK, AZURE_INSTANCE)
                } else {
                    (StatusCode::BAD_REQUEST, "")
                }
            }),
        );
    let (_runtime, base) = serve(app);
    assert_eq!(
        fetch(Provider::Gcp, &base, Duration::from_secs(2))
            .unwrap()
            .provider,
        "gcp"
    );
    assert_eq!(
        fetch(Provider::Azure, &base, Duration::from_secs(2))
            .unwrap()
            .provider,
        "azure"
    );
}

#[test]
fn a_silent_metadata_service_costs_one_timeout() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let started = Instant::now();
    assert_eq!(
        fetch(Provider::Gcp, &base, Duration::from_millis(100)),
        None
    );
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "{:?}",
        started.elapsed()
    );
    drop(listener);
}
