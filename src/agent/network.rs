use std::{
    collections::BTreeMap,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs as _, UdpSocket},
};

use serde::Serialize;

use super::host::Root;

pub const MAX_ADDRESSES: usize = 240;
pub const MAX_INTERFACES: usize = 128;

const IFA_F_TEMPORARY: u32 = 0x01;
const IFA_F_DEPRECATED: u32 = 0x20;
const IFA_F_TENTATIVE: u32 = 0x40;
const ARPHRD_LOOPBACK: u32 = 772;
const TUNNEL_TYPES: [u32; 5] = [65534, 768, 776, 778, 823];

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Loopback,
    Physical,
    Bridge,
    Bond,
    Vlan,
    Tunnel,
    Veth,
    Virtual,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Facts {
    pub index: u32,
    pub iflink: u32,
    pub link_type: u32,
    pub device: bool,
    pub bridge: bool,
    pub bonding: bool,
    pub devtype: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct V6 {
    pub address: Ipv6Addr,
    pub prefix: u8,
    pub flags: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Link {
    pub name: String,
    pub kind: Kind,
    pub up: bool,
    pub mac: Option<[u8; 6]>,
    pub mtu: Option<u32>,
    pub ipv4: Vec<(Ipv4Addr, u8)>,
    pub ipv6: Vec<V6>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Interface {
    pub name: String,
    pub kind: Kind,
    pub state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mac: Option<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub mac_local: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mtu: Option<u32>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub addresses: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Primary {
    pub address: IpAddr,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interface: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Network {
    pub interfaces: Vec<Interface>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub primary: Option<Primary>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub hidden: BTreeMap<&'static str, u32>,
    #[serde(skip_serializing_if = "is_zero")]
    pub addresses_left_out: u32,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub own_namespace: bool,
}

#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde passes a reference"
)]
const fn is_zero(count: &u32) -> bool {
    *count == 0
}

#[must_use]
pub fn classify(name: &str, facts: &Facts) -> Kind {
    let devtype = facts.devtype.as_deref();
    if facts.link_type == ARPHRD_LOOPBACK {
        Kind::Loopback
    } else if facts.bridge || devtype == Some("bridge") {
        Kind::Bridge
    } else if facts.bonding || devtype == Some("bond") {
        Kind::Bond
    } else if devtype == Some("vlan") {
        Kind::Vlan
    } else if TUNNEL_TYPES.contains(&facts.link_type)
        || matches!(
            devtype,
            Some("wireguard" | "ipip" | "gre" | "sit" | "vxlan")
        )
    {
        Kind::Tunnel
    } else if facts.device {
        Kind::Physical
    } else if ["veth", "cali", "lxc", "eni", "gke"]
        .iter()
        .any(|prefix| name.starts_with(prefix))
    {
        Kind::Veth
    } else {
        Kind::Virtual
    }
}

#[must_use]
pub fn container_bridge(name: &str) -> bool {
    let numbered = |prefix: &str| {
        name.strip_prefix(prefix)
            .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()))
    };
    matches!(
        name,
        "docker0" | "cni0" | "cbr0" | "kube-bridge" | "lxcbr0" | "lxdbr0"
    ) || numbered("virbr")
        || numbered("podman")
        || name
            .strip_prefix("br-")
            .is_some_and(|id| id.len() == 12 && id.bytes().all(|b| b.is_ascii_hexdigit()))
}

#[must_use]
pub fn format_mac(mac: [u8; 6]) -> String {
    mac.iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join(":")
}

const fn reported_v4(address: Ipv4Addr) -> bool {
    !address.is_link_local() && !address.is_loopback() && !address.is_unspecified()
}

fn reported_v6(v6: &V6) -> bool {
    let link_local = v6
        .address
        .segments()
        .first()
        .is_some_and(|first| first & 0xffc0 == 0xfe80);
    !link_local
        && !v6.address.is_loopback()
        && !v6.address.is_unspecified()
        && v6.flags & (IFA_F_TEMPORARY | IFA_F_DEPRECATED | IFA_F_TENTATIVE) == 0
}

pub fn report(mut links: Vec<Link>, in_container: bool, primary: Option<IpAddr>) -> Network {
    links.sort_by(|a, b| a.name.cmp(&b.name));
    let own_namespace = in_container
        && !links.iter().any(|link| link.kind == Kind::Physical)
        && links.iter().any(|link| link.kind == Kind::Veth);

    let mut network = Network {
        own_namespace,
        ..Network::default()
    };
    let mut budget = MAX_ADDRESSES;
    for link in links {
        let hidden = match link.kind {
            Kind::Loopback => continue,
            Kind::Veth if !own_namespace => continue,
            Kind::Bridge if container_bridge(&link.name) => Some("container_bridge"),
            _ => None,
        };
        if let Some(reason) = hidden {
            *network.hidden.entry(reason).or_default() += 1;
            continue;
        }
        if network.interfaces.len() == MAX_INTERFACES {
            *network.hidden.entry("left_out").or_default() += 1;
            continue;
        }

        let mut addresses: Vec<String> = link
            .ipv4
            .iter()
            .filter(|(address, _)| reported_v4(*address))
            .map(|(address, prefix)| format!("{address}/{prefix}"))
            .chain(
                link.ipv6
                    .iter()
                    .filter(|v6| reported_v6(v6))
                    .map(|v6| format!("{}/{}", v6.address, v6.prefix)),
            )
            .collect();
        if addresses.len() > budget {
            let left_out = addresses.len() - budget;
            network.addresses_left_out = network
                .addresses_left_out
                .saturating_add(u32::try_from(left_out).unwrap_or(u32::MAX));
            addresses.truncate(budget);
        }
        budget -= addresses.len();

        if network.primary.is_none()
            && let Some(primary) = primary
            && (link
                .ipv4
                .iter()
                .any(|(address, _)| IpAddr::V4(*address) == primary)
                || link.ipv6.iter().any(|v6| IpAddr::V6(v6.address) == primary))
        {
            network.primary = Some(Primary {
                address: primary,
                interface: Some(link.name.clone()),
            });
        }

        let mac = link.mac.filter(|mac| *mac != [0; 6]);
        network.interfaces.push(Interface {
            name: link.name,
            kind: link.kind,
            state: if link.up { "up" } else { "down" },
            mac_local: mac.is_some_and(|mac| mac.first().is_some_and(|byte| byte & 0x02 != 0)),
            mac: mac.map(format_mac),
            mtu: link.mtu,
            addresses,
        });
    }

    if network.primary.is_none()
        && let Some(primary) = primary
        && !primary.is_loopback()
    {
        network.primary = Some(Primary {
            address: primary,
            interface: None,
        });
    }
    network
}

#[must_use]
pub fn parse_if_inet6(text: &str) -> BTreeMap<String, Vec<V6>> {
    let mut found: BTreeMap<String, Vec<V6>> = BTreeMap::new();
    for line in text.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let [hex, _index, prefix, _scope, flags, name] = fields.as_slice() else {
            continue;
        };
        let Ok(bits) = u128::from_str_radix(hex, 16) else {
            continue;
        };
        if hex.len() != 32 {
            continue;
        }
        let (Ok(prefix), Ok(flags)) = (
            u8::from_str_radix(prefix, 16),
            u32::from_str_radix(flags, 16),
        ) else {
            continue;
        };
        found.entry((*name).to_owned()).or_default().push(V6 {
            address: Ipv6Addr::from(bits),
            prefix,
            flags,
        });
    }
    found
}

#[must_use]
pub fn parse_mac(text: &str) -> Option<[u8; 6]> {
    let mut mac = [0; 6];
    let mut parts = text.trim().split(':');
    for byte in &mut mac {
        let part = parts.next()?;
        if part.len() != 2 || !part.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        *byte = u8::from_str_radix(part, 16).ok()?;
    }
    parts.next().is_none().then_some(mac)
}

#[must_use]
pub fn default_route_interface(route: &str) -> Option<String> {
    route.lines().skip(1).find_map(|line| {
        let fields: Vec<&str> = line.split_whitespace().collect();
        match fields.as_slice() {
            [
                name,
                "00000000",
                _gateway,
                _flags,
                _refs,
                _use,
                _metric,
                "00000000",
                ..,
            ] => Some((*name).to_owned()),
            _ => None,
        }
    })
}

fn read_ipv4() -> BTreeMap<String, Vec<(Ipv4Addr, u8)>> {
    let mut found: BTreeMap<String, Vec<(Ipv4Addr, u8)>> = BTreeMap::new();
    let Ok(addresses) = nix::ifaddrs::getifaddrs() else {
        return found;
    };
    for entry in addresses {
        let Some(address) = entry.address.as_ref().and_then(|a| a.as_sockaddr_in()) else {
            continue;
        };
        let prefix = entry
            .netmask
            .as_ref()
            .and_then(|mask| mask.as_sockaddr_in())
            .map_or(32, |mask| {
                u8::try_from(u32::from(mask.ip()).count_ones()).unwrap_or(32)
            });
        found
            .entry(entry.interface_name)
            .or_default()
            .push((address.ip(), prefix));
    }
    found
}

#[must_use]
pub fn links(root: &Root) -> Vec<Link> {
    let mut ipv4 = read_ipv4();
    let mut ipv6 = root
        .read("/proc/net/if_inet6")
        .map(|text| parse_if_inet6(&text))
        .unwrap_or_default();
    let Ok(entries) = std::fs::read_dir(root.path("/sys/class/net")) else {
        return Vec::new();
    };

    let mut links = Vec::new();
    for entry in entries.flatten() {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        let at = |file: &str| root.read(&format!("/sys/class/net/{name}/{file}"));
        let number = |file: &str| at(file).and_then(|text| text.parse::<u32>().ok());
        let facts = Facts {
            index: number("ifindex").unwrap_or_default(),
            iflink: number("iflink").unwrap_or_default(),
            link_type: number("type").unwrap_or_default(),
            device: root.exists(&format!("/sys/class/net/{name}/device")),
            bridge: root.exists(&format!("/sys/class/net/{name}/bridge")),
            bonding: root.exists(&format!("/sys/class/net/{name}/bonding")),
            devtype: at("uevent").and_then(|uevent| {
                uevent
                    .lines()
                    .find_map(|line| line.strip_prefix("DEVTYPE=").map(str::to_owned))
            }),
        };
        links.push(Link {
            kind: classify(&name, &facts),
            up: matches!(at("operstate").as_deref(), Some("up" | "unknown")),
            mac: at("address").and_then(|text| parse_mac(&text)),
            mtu: number("mtu"),
            ipv4: ipv4.remove(&name).unwrap_or_default(),
            ipv6: ipv6.remove(&name).unwrap_or_default(),
            name,
        });
    }
    links
}

#[must_use]
pub fn route_source(target: &str, links: &[Link], root: &Root) -> Option<IpAddr> {
    let by_route = target
        .to_socket_addrs()
        .ok()
        .into_iter()
        .flatten()
        .find_map(source_for)
        .filter(|source| !source.is_loopback());
    by_route.or_else(|| {
        let name = default_route_interface(&root.read("/proc/net/route")?)?;
        let link = links.iter().find(|link| link.name == name)?;
        link.ipv4.first().map(|(address, _)| IpAddr::V4(*address))
    })
}

fn source_for(target: SocketAddr) -> Option<IpAddr> {
    let local: SocketAddr = if target.is_ipv4() {
        (Ipv4Addr::UNSPECIFIED, 0).into()
    } else {
        (Ipv6Addr::UNSPECIFIED, 0).into()
    };
    let socket = UdpSocket::bind(local).ok()?;
    socket.connect(target).ok()?;
    let source = socket.local_addr().ok()?.ip();
    (!source.is_unspecified()).then_some(source)
}

#[cfg(test)]
mod tests;
