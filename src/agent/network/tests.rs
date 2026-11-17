use std::net::{IpAddr, Ipv4Addr};

use proptest::prelude::*;

use super::{
    Facts, Kind, Link, MAX_ADDRESSES, V6, classify, container_bridge, default_route_interface,
    parse_if_inet6, parse_mac, report,
};

fn link(name: &str, kind: Kind, ipv4: &[(&str, u8)], ipv6: &[(&str, u8, u32)]) -> Link {
    Link {
        name: name.to_owned(),
        kind,
        up: true,
        mac: Some([0x0a, 0x3f, 0x91, 0x2c, 0x7e, 0x01]),
        mtu: Some(1500),
        ipv4: ipv4
            .iter()
            .map(|(ip, prefix)| (ip.parse().unwrap(), *prefix))
            .collect(),
        ipv6: ipv6
            .iter()
            .map(|(ip, prefix, flags)| V6 {
                address: ip.parse().unwrap(),
                prefix: *prefix,
                flags: *flags,
            })
            .collect(),
    }
}

#[test]
fn the_workstation_in_the_plan_reports_what_the_plan_says() {
    let mut wifi = link("wlp4s0", Kind::Physical, &[], &[]);
    wifi.up = false;
    wifi.mac = Some([0x5a, 0x11, 0x22, 0x33, 0x44, 0x55]);
    let links = vec![
        link(
            "lo",
            Kind::Loopback,
            &[("127.0.0.1", 8)],
            &[("::1", 128, 0x80)],
        ),
        link(
            "enp5s0f1",
            Kind::Physical,
            &[("192.168.86.31", 24)],
            &[
                ("fe80::1", 64, 0x80),
                ("2601:1:2:3::31", 64, 0x00),
                ("2601:1:2:3:aaaa::1", 64, 0x01),
                ("2601:1:2:3:bbbb::1", 64, 0x20),
            ],
        ),
        link(
            "tailscale0",
            Kind::Tunnel,
            &[("100.86.210.72", 32)],
            &[("fd7a:115c:a1e0::8a39:d249", 128, 0x80)],
        ),
        wifi,
        link("docker0", Kind::Bridge, &[("172.17.0.1", 16)], &[]),
        link("br-0123456789ab", Kind::Bridge, &[("172.18.0.1", 16)], &[]),
        link("veth8573604", Kind::Veth, &[], &[("fe80::2", 64, 0x80)]),
        link("br0", Kind::Bridge, &[("10.9.0.1", 24)], &[]),
    ];
    let network = report(links, false, Some("192.168.86.31".parse().unwrap()));

    let names: Vec<&str> = network.interfaces.iter().map(|i| i.name.as_str()).collect();
    assert_eq!(names, ["br0", "enp5s0f1", "tailscale0", "wlp4s0"]);
    assert_eq!(
        network.interfaces[1].addresses,
        ["192.168.86.31/24", "2601:1:2:3::31/64"]
    );
    assert_eq!(
        network.interfaces[2].addresses,
        ["100.86.210.72/32", "fd7a:115c:a1e0::8a39:d249/128"]
    );
    assert_eq!(network.interfaces[3].state, "down");
    assert!(network.interfaces[3].mac_local);
    assert!(network.interfaces[3].addresses.is_empty());
    assert_eq!(network.hidden.get("veth"), None);
    assert_eq!(network.hidden["container_bridge"], 2);
    let primary = network.primary.unwrap();
    assert_eq!(primary.interface.as_deref(), Some("enp5s0f1"));
    assert!(!network.own_namespace);
}

#[test]
fn a_container_without_host_networking_says_so() {
    let links = vec![
        link("lo", Kind::Loopback, &[("127.0.0.1", 8)], &[]),
        link("eth0", Kind::Veth, &[("10.244.1.17", 24)], &[]),
    ];
    let network = report(links.clone(), true, Some("10.244.1.17".parse().unwrap()));
    assert!(network.own_namespace);
    assert_eq!(network.interfaces[0].name, "eth0");
    assert!(network.hidden.is_empty());

    assert!(!report(links, false, None).own_namespace);
}

#[test]
fn many_addresses_are_capped_and_counted() {
    let many: Vec<(String, u8)> = (0..300)
        .map(|n| (format!("10.1.{}.{}", n / 250, n % 250 + 1), 32))
        .collect();
    let borrowed: Vec<(&str, u8)> = many
        .iter()
        .map(|(ip, prefix)| (ip.as_str(), *prefix))
        .collect();
    let network = report(
        vec![link("eth0", Kind::Physical, &borrowed, &[])],
        false,
        None,
    );
    assert_eq!(network.interfaces[0].addresses.len(), MAX_ADDRESSES);
    assert_eq!(network.addresses_left_out, 60);
}

#[test]
fn many_interfaces_are_capped_and_counted() {
    let links = (0..200)
        .map(|n| link(&format!("eth{n:03}"), Kind::Physical, &[], &[]))
        .collect();
    let network = report(links, false, None);
    assert_eq!(network.interfaces.len(), super::MAX_INTERFACES);
    assert_eq!(network.hidden["left_out"], 72);
}

#[test]
fn a_primary_on_no_interface_is_still_reported() {
    let network = report(Vec::new(), false, Some("203.0.113.9".parse().unwrap()));
    assert_eq!(network.primary.unwrap().interface, None);
    assert_eq!(
        report(Vec::new(), false, Some(IpAddr::V4(Ipv4Addr::LOCALHOST))).primary,
        None
    );
}

#[test]
fn interface_kinds_come_from_sysfs() {
    let facts = |link_type: u32, device: bool, devtype: Option<&str>| Facts {
        index: 2,
        iflink: 2,
        link_type,
        device,
        devtype: devtype.map(str::to_owned),
        ..Facts::default()
    };
    assert_eq!(classify("lo", &facts(772, false, None)), Kind::Loopback);
    assert_eq!(classify("eth0", &facts(1, true, None)), Kind::Physical);
    assert_eq!(
        classify("wlan0", &facts(1, true, Some("wlan"))),
        Kind::Physical
    );
    assert_eq!(
        classify("br0", &facts(1, false, Some("bridge"))),
        Kind::Bridge
    );
    assert_eq!(
        classify("eth0.30", &facts(1, false, Some("vlan"))),
        Kind::Vlan
    );
    assert_eq!(
        classify("wg0", &facts(65534, false, Some("wireguard"))),
        Kind::Tunnel
    );
    assert_eq!(classify("tun0", &facts(65534, false, None)), Kind::Tunnel);
    assert_eq!(classify("veth12ab", &facts(1, false, None)), Kind::Veth);
    assert_eq!(classify("cali0f3a", &facts(1, false, None)), Kind::Veth);
    assert_eq!(
        classify("eni1a2b3c4d5e6", &facts(1, false, None)),
        Kind::Veth
    );
    assert_eq!(
        classify("gke0a1b2c3d4e5", &facts(1, false, None)),
        Kind::Veth
    );
    assert_eq!(classify("dummy0", &facts(1, false, None)), Kind::Virtual);
    let bond = Facts {
        bonding: true,
        ..facts(1, false, None)
    };
    assert_eq!(classify("bond0", &bond), Kind::Bond);
}

#[test]
fn container_bridges_are_told_from_real_ones() {
    for name in [
        "docker0",
        "br-0123456789ab",
        "cni0",
        "virbr0",
        "podman1",
        "kube-bridge",
    ] {
        assert!(container_bridge(name), "{name}");
    }
    for name in [
        "br0",
        "br-lan",
        "br-0123456789abc",
        "virbr",
        "vmbr0",
        "podmanx",
    ] {
        assert!(!container_bridge(name), "{name}");
    }
}

#[test]
fn proc_files_parse() {
    let inet6 = "\
fe800000000000000a3f91fffe2c7e01 02 40 20 80     eth0
260100010002000300000000000000310 02 40 00 00     eth0
26010001000200030000000000000031 02 40 00 01     eth0
00000000000000000000000000000001 01 80 10 80       lo
garbage
";
    let parsed = parse_if_inet6(inet6);
    assert_eq!(parsed["eth0"].len(), 2);
    assert_eq!(
        parsed["eth0"][1].address,
        "2601:1:2:3::31".parse::<std::net::Ipv6Addr>().unwrap()
    );
    assert_eq!(parsed["eth0"][1].flags, 1);
    assert_eq!(parsed["lo"][0].prefix, 128);

    let route = "\
Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT
docker0\t000011AC\t00000000\t0001\t0\t0\t0\t0000FFFF\t0\t0\t0
enp5s0f1\t00000000\t0156A8C0\t0003\t0\t0\t100\t00000000\t0\t0\t0
";
    assert_eq!(default_route_interface(route).as_deref(), Some("enp5s0f1"));
    assert_eq!(default_route_interface("Iface\n"), None);

    assert_eq!(
        parse_mac("0a:3f:91:2c:7e:01\n"),
        Some([0x0a, 0x3f, 0x91, 0x2c, 0x7e, 0x01])
    );
    for bad in [
        "",
        "0a:3f",
        "0a:3f:91:2c:7e:01:02",
        "+a:3f:91:2c:7e:01",
        "0a-3f-91-2c-7e-01",
    ] {
        assert_eq!(parse_mac(bad), None, "{bad}");
    }
}

#[test]
fn the_route_to_a_local_address_comes_from_loopback() {
    let root = super::Root::system();
    let source = super::route_source("127.0.0.1:9", &[], &root);
    assert_eq!(source, Some(IpAddr::V4(Ipv4Addr::LOCALHOST)));
}

fn any_link() -> impl Strategy<Value = Link> {
    let kind = prop_oneof![
        Just(Kind::Loopback),
        Just(Kind::Physical),
        Just(Kind::Bridge),
        Just(Kind::Veth),
        Just(Kind::Tunnel),
        Just(Kind::Virtual),
    ];
    (
        "[a-z]{1,6}[0-9]{0,2}",
        kind,
        any::<bool>(),
        proptest::option::of(any::<[u8; 6]>()),
        proptest::collection::vec((any::<u32>(), 0_u8..=32), 0..40),
        proptest::collection::vec((any::<u128>(), 0_u8..=128, any::<u32>()), 0..40),
    )
        .prop_map(|(name, kind, up, mac, ipv4, ipv6)| Link {
            name,
            kind,
            up,
            mac,
            mtu: None,
            ipv4: ipv4
                .into_iter()
                .map(|(ip, prefix)| (Ipv4Addr::from(ip), prefix))
                .collect(),
            ipv6: ipv6
                .into_iter()
                .map(|(ip, prefix, flags)| V6 {
                    address: ip.into(),
                    prefix,
                    flags,
                })
                .collect(),
        })
}

proptest! {
    #[test]
    fn a_report_never_lists_noise_or_exceeds_its_budget(
        links in proptest::collection::vec(any_link(), 0..12),
        in_container in any::<bool>(),
    ) {
        let network = report(links.clone(), in_container, None);
        let listed: usize = network.interfaces.iter().map(|i| i.addresses.len()).sum();
        prop_assert!(listed <= MAX_ADDRESSES);
        prop_assert!(network.interfaces.len() <= super::MAX_INTERFACES);

        for interface in &network.interfaces {
            prop_assert_ne!(interface.kind, Kind::Loopback);
            prop_assert!(network.own_namespace || interface.kind != Kind::Veth);
            for address in &interface.addresses {
                let ip: IpAddr = address.split('/').next().unwrap().parse().unwrap();
                prop_assert!(!ip.is_loopback() && !ip.is_unspecified());
                if let IpAddr::V6(v6) = ip {
                    prop_assert_ne!(v6.segments()[0] & 0xffc0, 0xfe80);
                }
            }
        }
        prop_assert_eq!(&report(links, in_container, None), &network);
    }
}

#[test]
fn an_eks_node_lists_its_enis_and_hides_pod_veths() {
    let mut links = vec![
        link("lo", Kind::Loopback, &[("127.0.0.1", 8)], &[]),
        link(
            "ens5",
            Kind::Physical,
            &[("10.0.3.17", 20)],
            &[("fe80::83f:91ff:fe2c:7e01", 64, 0x80)],
        ),
        link(
            "ens6",
            Kind::Physical,
            &[("10.0.3.88", 20), ("10.0.3.89", 20), ("10.0.3.90", 20)],
            &[],
        ),
    ];
    links.extend((0..30).map(|n| link(&format!("eni{n:011x}"), Kind::Veth, &[], &[])));
    let network = report(links, false, Some("10.0.3.17".parse().unwrap()));
    let names: Vec<&str> = network.interfaces.iter().map(|i| i.name.as_str()).collect();
    assert_eq!(names, ["ens5", "ens6"]);
    assert_eq!(network.interfaces[1].addresses.len(), 3);
    assert!(network.hidden.is_empty());
    assert_eq!(network.primary.unwrap().interface.as_deref(), Some("ens5"));
}

#[test]
fn a_gke_node_keeps_its_vm_address_and_drops_the_cni_bridge() {
    let links = vec![
        link("lo", Kind::Loopback, &[("127.0.0.1", 8)], &[]),
        link("eth0", Kind::Physical, &[("10.128.0.2", 32)], &[]),
        link("cbr0", Kind::Bridge, &[("10.4.0.1", 24)], &[]),
        link("gke0a1b2c3d4e5", Kind::Veth, &[], &[]),
        link("veth9a8b7c6d", Kind::Veth, &[], &[]),
    ];
    let network = report(links, false, Some("10.128.0.2".parse().unwrap()));
    let names: Vec<&str> = network.interfaces.iter().map(|i| i.name.as_str()).collect();
    assert_eq!(names, ["eth0"]);
    assert_eq!(network.hidden["container_bridge"], 1);
    assert_eq!(network.hidden.len(), 1);
}

#[test]
fn a_host_with_a_bond_and_vlans_lists_every_layer() {
    let mut down = link("eno2", Kind::Physical, &[], &[]);
    down.up = false;
    let links = vec![
        link("eno1", Kind::Physical, &[], &[]),
        down,
        link("bond0", Kind::Bond, &[], &[]),
        link(
            "bond0.30",
            Kind::Vlan,
            &[("10.30.0.5", 24)],
            &[("2001:db8:30::5", 64, 0x00)],
        ),
        link("bond0.40", Kind::Vlan, &[("10.40.0.5", 24)], &[]),
        link("br0", Kind::Bridge, &[("192.168.40.3", 24)], &[]),
    ];
    let network = report(links, false, Some("10.30.0.5".parse().unwrap()));
    let names: Vec<&str> = network.interfaces.iter().map(|i| i.name.as_str()).collect();
    assert_eq!(
        names,
        ["bond0", "bond0.30", "bond0.40", "br0", "eno1", "eno2"]
    );
    assert_eq!(network.interfaces[1].kind, Kind::Vlan);
    assert_eq!(network.interfaces[5].state, "down");
    assert_eq!(
        network.primary.unwrap().interface.as_deref(),
        Some("bond0.30")
    );
    assert!(network.hidden.is_empty());
}

proptest! {
    #[test]
    fn container_churn_and_rotating_ipv6_never_change_the_report(
        base in proptest::collection::vec(any_link(), 0..8),
        veths in proptest::collection::vec(("[a-f0-9]{8}", proptest::collection::vec(any::<u128>(), 0..3)), 0..12),
        rotating in proptest::collection::vec(
            (0_usize..8, any::<u128>(), prop_oneof![Just(0x01_u32), Just(0x20), Just(0x40), Just(0x21)]),
            0..12,
        ),
    ) {
        let base: Vec<Link> = base.into_iter().filter(|link| link.kind != Kind::Veth).collect();
        let before = serde_json::to_vec(&report(base.clone(), false, None)).unwrap();

        let mut churned = base;
        if !churned.is_empty() {
            let count = churned.len();
            for (index, bits, flags) in rotating {
                churned[index % count].ipv6.push(V6 { address: bits.into(), prefix: 64, flags });
            }
        }
        for (name, addresses) in veths {
            churned.push(Link {
                name: format!("veth{name}"),
                kind: Kind::Veth,
                up: true,
                mac: Some([2, 0, 0, 0, 0, 1]),
                mtu: Some(1500),
                ipv4: Vec::new(),
                ipv6: addresses
                    .into_iter()
                    .map(|bits| V6 { address: bits.into(), prefix: 64, flags: 0 })
                    .collect(),
            });
        }
        let after = serde_json::to_vec(&report(churned, false, None)).unwrap();
        prop_assert_eq!(String::from_utf8(before).unwrap(), String::from_utf8(after).unwrap());
    }
}
