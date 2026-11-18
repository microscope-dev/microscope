use std::collections::BTreeMap;

use serde_json::json;

use super::{AGENT_VERSION, MAX_BYTES, encode, gather};
use crate::agent::{
    cloud::Cloud,
    config::load,
    host::Root,
    network::{Interface, Kind, Network},
    scratch::Scratch,
};

const ENROLL: &str =
    "https://mse_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaasnb47oni@app.microscope.dev";

fn config(extra: &[(&'static str, &'static str)]) -> crate::agent::config::Config {
    let mut env: BTreeMap<&str, &str> = BTreeMap::from([
        ("MICROSCOPE_ENROLL", ENROLL),
        ("MICROSCOPE_TAGS", "environment=prod,team=payments"),
    ]);
    env.extend(extra.iter().copied());
    load(Some(std::path::Path::new("/dev/null")), &env).unwrap()
}

fn root() -> Scratch {
    let scratch = Scratch::new();
    for (path, text) in [
        (
            "proc/sys/kernel/hostname",
            "ip-10-0-3-17.us-west-2.compute.internal\n",
        ),
        ("proc/sys/kernel/osrelease", "6.8.0-1015-aws\n"),
        ("etc/machine-id", "4f2c9a7e1b0d4c3e8a5f6b7c8d9e0f1a\n"),
        ("proc/self/status", "CapEff:\t000001ffffffffff\n"),
    ] {
        let path = scratch.path().join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    scratch
}

fn cloud() -> Cloud {
    Cloud {
        provider: "aws",
        instance_id: Some("i-0a1b2c3d4e5f60718".into()),
        private_ips: vec!["10.0.3.17".parse().unwrap()],
        public_ips: vec!["203.0.113.24".parse().unwrap()],
        ..Cloud::default()
    }
}

#[test]
fn the_inventory_has_the_shape_services_reads() {
    let scratch = root();
    let config = config(&[
        ("MICROSCOPE_NODE_NAME", "ip-10-0-3-17"),
        ("MICROSCOPE_NODE_IPS", "10.0.3.17"),
    ]);
    let mut inventory = gather(
        &config,
        &Root::at(scratch.path()),
        Some(&cloud()),
        "127.0.0.1:9",
    );
    let value: serde_json::Value = serde_json::from_slice(&encode(&mut inventory)).unwrap();

    assert_eq!(value["hostname"], "ip-10-0-3-17.us-west-2.compute.internal");
    assert_eq!(value["agent_version"], AGENT_VERSION);
    assert_eq!(value["key_algs"], json!(["ed25519"]));
    assert_eq!(
        value["tags"],
        json!({ "environment": "prod", "team": "payments" })
    );
    assert_eq!(value.get("environment"), None);
    assert!(
        value["host_ids"][0]
            .as_str()
            .unwrap()
            .starts_with("machine-id:")
    );
    assert_eq!(value["cloud"]["instance_id"], "i-0a1b2c3d4e5f60718");
    assert_eq!(value["cloud"]["public_ips"], json!(["203.0.113.24"]));
    assert_eq!(
        value["kubernetes"],
        json!({ "node": "ip-10-0-3-17", "node_ips": ["10.0.3.17"] })
    );
    assert_eq!(value["system"]["kernel"], "6.8.0-1015-aws");
    assert_eq!(
        value["capabilities"],
        json!(["bpf", "perfmon", "sys_ptrace", "sys_admin"])
    );
    assert_eq!(
        value["container"],
        json!({ "runtime": "kubernetes", "host_pids": false })
    );
    assert!(value["network"].is_object());
}

#[test]
fn report_network_false_sends_no_addresses() {
    let scratch = root();
    let config = config(&[
        ("MICROSCOPE_REPORT_NETWORK", "false"),
        ("MICROSCOPE_NODE_IPS", "10.0.3.17"),
    ]);
    let mut inventory = gather(
        &config,
        &Root::at(scratch.path()),
        Some(&cloud()),
        "127.0.0.1:9",
    );
    let value: serde_json::Value = serde_json::from_slice(&encode(&mut inventory)).unwrap();
    assert_eq!(value.get("network"), None);
    assert_eq!(value["cloud"].get("private_ips"), None);
    assert_eq!(value["cloud"].get("public_ips"), None);
    assert_eq!(value.get("kubernetes"), Some(&json!({})));
    assert_eq!(value["cloud"]["instance_id"], "i-0a1b2c3d4e5f60718");
}

#[test]
fn a_missing_or_strange_hostname_becomes_localhost() {
    let scratch = Scratch::new();
    let config = config(&[]);
    assert_eq!(
        gather(&config, &Root::at(scratch.path()), None, "127.0.0.1:9").hostname,
        "localhost"
    );
    std::fs::create_dir_all(scratch.path().join("proc/sys/kernel")).unwrap();
    std::fs::write(scratch.path().join("proc/sys/kernel/hostname"), "two words").unwrap();
    assert_eq!(
        gather(&config, &Root::at(scratch.path()), None, "127.0.0.1:9").hostname,
        "localhost"
    );
}

#[test]
fn an_inventory_too_big_for_services_sheds_interfaces() {
    let scratch = root();
    let mut inventory = gather(&config(&[]), &Root::at(scratch.path()), None, "127.0.0.1:9");
    inventory.network = Some(Network {
        interfaces: (0..128)
            .map(|n| Interface {
                name: format!("eth{n}"),
                kind: Kind::Physical,
                state: "up",
                mac: Some("0a:00:00:00:00:01".into()),
                mac_local: true,
                mtu: Some(9001),
                addresses: (0..6)
                    .map(|m| format!("fd00:{n:x}:{m:x}:ffff:ffff:ffff:ffff:ffff/64"))
                    .collect(),
            })
            .collect(),
        ..Network::default()
    });

    let bytes = encode(&mut inventory);
    assert!(bytes.len() <= MAX_BYTES, "{}", bytes.len());
    let network = inventory.network.unwrap();
    assert!(network.interfaces.len() < 128);
    assert_eq!(
        usize::try_from(network.hidden["left_out"]).unwrap() + network.interfaces.len(),
        128
    );
    assert!(network.addresses_left_out > 0);
}

#[test]
fn an_inventory_too_big_without_a_network_drops_its_tags() {
    let scratch = root();
    let mut inventory = gather(
        &config(&[("MICROSCOPE_REPORT_NETWORK", "false")]),
        &Root::at(scratch.path()),
        None,
        "127.0.0.1:9",
    );
    inventory.tags = (0..200)
        .map(|n| (format!("t{n}"), "v".repeat(256)))
        .collect();

    let bytes = encode(&mut inventory);
    assert!(bytes.len() <= MAX_BYTES, "{}", bytes.len());
    assert!(inventory.tags.is_empty());
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(value.get("tags"), None);
}
