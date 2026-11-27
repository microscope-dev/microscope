//! Gates `169.254.169.254` metadata probes behind DMI/hypervisor hints so
//! bare-metal hosts never wait on link-local timeouts.

use std::{net::IpAddr, time::Duration};

use serde::{Deserialize, Serialize};

use super::host::Root;

pub const METADATA: &str = "http://169.254.169.254";
pub const TIMEOUT: Duration = Duration::from_millis(200);
const BODY_LIMIT: u64 = 64 * 1024;
const AZURE_ASSET_TAG: &str = "7783-7084-3265-9085-8269-3286-77";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Provider {
    Aws,
    Gcp,
    Azure,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Cloud {
    pub provider: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub zone: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instance_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instance_id: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub private_ips: Vec<IpAddr>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub public_ips: Vec<IpAddr>,
}

#[must_use]
pub fn detect(root: &Root) -> Option<Provider> {
    let dmi = |name: &str| {
        root.read(&format!("/sys/class/dmi/id/{name}"))
            .unwrap_or_default()
    };
    if dmi("sys_vendor") == "Amazon EC2"
        || dmi("board_vendor") == "Amazon EC2"
        || root
            .read("/sys/hypervisor/uuid")
            .is_some_and(|uuid| uuid.to_ascii_lowercase().starts_with("ec2"))
    {
        Some(Provider::Aws)
    } else if dmi("product_name") == "Google Compute Engine" || dmi("sys_vendor") == "Google" {
        Some(Provider::Gcp)
    } else if dmi("chassis_asset_tag") == AZURE_ASSET_TAG {
        Some(Provider::Azure)
    } else {
        None
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AwsDocument {
    instance_id: Option<String>,
    instance_type: Option<String>,
    region: Option<String>,
    availability_zone: Option<String>,
    private_ip: Option<IpAddr>,
}

#[must_use]
pub fn parse_aws(document: &str, public_ip: Option<&str>) -> Option<Cloud> {
    let document: AwsDocument = serde_json::from_str(document).ok()?;
    let instance_id = document.instance_id.filter(|id| !id.is_empty())?;
    Some(Cloud {
        provider: "aws",
        region: document.region,
        zone: document.availability_zone,
        instance_type: document.instance_type,
        instance_id: Some(instance_id),
        private_ips: document.private_ip.into_iter().collect(),
        public_ips: public_ip
            .and_then(|ip| ip.trim().parse().ok())
            .into_iter()
            .collect(),
    })
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GcpInstance {
    id: Option<serde_json::Value>,
    zone: Option<String>,
    machine_type: Option<String>,
    #[serde(default)]
    network_interfaces: Vec<GcpInterface>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GcpInterface {
    ip: Option<IpAddr>,
    #[serde(default)]
    access_configs: Vec<GcpAccess>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GcpAccess {
    external_ip: Option<IpAddr>,
}

fn last_segment(path: Option<String>) -> Option<String> {
    path.and_then(|path| path.rsplit('/').next().map(str::to_owned))
        .filter(|segment| !segment.is_empty())
}

#[must_use]
pub fn parse_gcp(instance: &str) -> Option<Cloud> {
    let instance: GcpInstance = serde_json::from_str(instance).ok()?;
    let zone = last_segment(instance.zone);
    let instance_id = instance.id.and_then(|id| match id {
        serde_json::Value::Number(number) => Some(number.to_string()),
        serde_json::Value::String(text) if !text.is_empty() => Some(text),
        _ => None,
    })?;
    Some(Cloud {
        provider: "gcp",
        region: zone
            .as_deref()
            .and_then(|zone| zone.rsplit_once('-'))
            .map(|(region, _)| region.to_owned()),
        zone,
        instance_type: last_segment(instance.machine_type),
        instance_id: Some(instance_id),
        private_ips: instance
            .network_interfaces
            .iter()
            .filter_map(|interface| interface.ip)
            .collect(),
        public_ips: instance
            .network_interfaces
            .iter()
            .flat_map(|interface| &interface.access_configs)
            .filter_map(|access| access.external_ip)
            .collect(),
    })
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AzureInstance {
    compute: AzureCompute,
    network: Option<AzureNetwork>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AzureCompute {
    vm_id: Option<String>,
    location: Option<String>,
    zone: Option<String>,
    vm_size: Option<String>,
}

#[derive(Deserialize)]
struct AzureNetwork {
    #[serde(default)]
    interface: Vec<AzureInterface>,
}

#[derive(Deserialize)]
struct AzureInterface {
    ipv4: Option<AzureAddresses>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AzureAddresses {
    #[serde(default)]
    ip_address: Vec<AzureAddress>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AzureAddress {
    private_ip_address: Option<String>,
    public_ip_address: Option<String>,
}

#[must_use]
pub fn parse_azure(instance: &str) -> Option<Cloud> {
    let instance: AzureInstance = serde_json::from_str(instance).ok()?;
    let addresses: Vec<&AzureAddress> = instance
        .network
        .iter()
        .flat_map(|network| &network.interface)
        .filter_map(|interface| interface.ipv4.as_ref())
        .flat_map(|ipv4| &ipv4.ip_address)
        .collect();
    let ips = |pick: fn(&AzureAddress) -> Option<&String>| {
        addresses
            .iter()
            .filter_map(|address| pick(address)?.parse().ok())
            .collect()
    };
    let compute = instance.compute;
    let instance_id = compute.vm_id.filter(|id| !id.is_empty())?;
    Some(Cloud {
        provider: "azure",
        region: compute.location,
        zone: compute.zone.filter(|zone| !zone.is_empty()),
        instance_type: compute.vm_size,
        instance_id: Some(instance_id),
        private_ips: ips(|address| address.private_ip_address.as_ref()),
        public_ips: ips(|address| address.public_ip_address.as_ref()),
    })
}

fn agent(timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .proxy(None)
        .max_redirects(0)
        .build()
        .new_agent()
}

fn text(response: Result<ureq::http::Response<ureq::Body>, ureq::Error>) -> Option<String> {
    response
        .ok()?
        .body_mut()
        .with_config()
        .limit(BODY_LIMIT)
        .read_to_string()
        .ok()
}

#[must_use]
pub fn fetch(provider: Provider, base: &str, timeout: Duration) -> Option<Cloud> {
    let agent = agent(timeout);
    match provider {
        Provider::Aws => {
            let token = text(
                agent
                    .put(format!("{base}/latest/api/token"))
                    .header("X-aws-ec2-metadata-token-ttl-seconds", "60")
                    .send_empty(),
            )?;
            let get = |path: &str| {
                text(
                    agent
                        .get(format!("{base}/latest/{path}"))
                        .header("X-aws-ec2-metadata-token", token.trim())
                        .call(),
                )
            };
            let document = get("dynamic/instance-identity/document")?;
            parse_aws(&document, get("meta-data/public-ipv4").as_deref())
        }
        Provider::Gcp => parse_gcp(&text(
            agent
                .get(format!(
                    "{base}/computeMetadata/v1/instance/?recursive=true"
                ))
                .header("Metadata-Flavor", "Google")
                .call(),
        )?),
        Provider::Azure => parse_azure(&text(
            agent
                .get(format!("{base}/metadata/instance?api-version=2021-02-01"))
                .header("Metadata", "true")
                .call(),
        )?),
    }
}

#[cfg(test)]
mod tests;
