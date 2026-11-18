use std::{collections::BTreeMap, net::IpAddr};

use serde::Serialize;

use super::{
    cloud::Cloud,
    config::Config,
    host::{self, Container, Root, System},
    network::{self, Network},
};

pub const MAX_BYTES: usize = 32 * 1024;
pub const AGENT_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Kubernetes {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub node_ips: Vec<IpAddr>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Inventory {
    pub hostname: String,
    pub agent_version: &'static str,
    pub key_algs: [&'static str; 1],
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub tags: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub host_ids: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cloud: Option<Cloud>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kubernetes: Option<Kubernetes>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub network: Option<Network>,
    pub system: System,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<Vec<&'static str>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container: Option<Container>,
}

fn valid_hostname(name: &str) -> bool {
    (1..=253).contains(&name.len()) && !name.chars().any(|c| c.is_control() || c.is_whitespace())
}

#[must_use]
pub fn gather(
    config: &Config,
    root: &Root,
    cloud: Option<&Cloud>,
    route_target: &str,
) -> Inventory {
    let in_kubernetes =
        config.node.is_some() || std::env::var_os("KUBERNETES_SERVICE_HOST").is_some();
    let container = host::container(root, in_kubernetes);

    let network = config.report_network.then(|| {
        let links = network::links(root);
        let primary = network::route_source(route_target, &links, root);
        network::report(links, container.is_some(), primary)
    });

    let kubernetes = (config.node.is_some() || !config.node_ips.is_empty()).then(|| Kubernetes {
        node: config.node.clone(),
        node_ips: config.node_ips.clone(),
    });

    let mut cloud = cloud.cloned();
    if !config.report_network
        && let Some(cloud) = &mut cloud
    {
        cloud.private_ips.clear();
        cloud.public_ips.clear();
    }

    Inventory {
        hostname: host::hostname(root)
            .filter(|name| valid_hostname(name))
            .unwrap_or_else(|| "localhost".to_owned()),
        agent_version: AGENT_VERSION,
        key_algs: ["ed25519"],
        tags: config.tags.clone(),
        host_ids: host::host_ids(root),
        cloud,
        kubernetes: if config.report_network {
            kubernetes
        } else {
            kubernetes.map(|kubernetes| Kubernetes {
                node_ips: Vec::new(),
                ..kubernetes
            })
        },
        network,
        system: host::system(root),
        capabilities: host::capabilities(root),
        container,
    }
}

pub fn encode(inventory: &mut Inventory) -> Vec<u8> {
    loop {
        let bytes = serde_json::to_vec(inventory).unwrap_or_default();
        if bytes.len() <= MAX_BYTES {
            return bytes;
        }
        let Some(network) = &mut inventory.network else {
            inventory.tags.clear();
            inventory.host_ids.clear();
            return serde_json::to_vec(inventory).unwrap_or_default();
        };
        match network.interfaces.pop() {
            Some(dropped) => {
                let addresses = u32::try_from(dropped.addresses.len()).unwrap_or(u32::MAX);
                network.addresses_left_out = network.addresses_left_out.saturating_add(addresses);
                *network.hidden.entry("left_out").or_default() += 1;
            }
            None => inventory.network = None,
        }
    }
}

#[cfg(test)]
mod tests;
