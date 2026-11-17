#![no_main]

use libfuzzer_sys::fuzz_target;
use microscope::agent::{
    cloud, network,
    wire::{EnrollKey, ErrorBody, KeyPair, Server, TokenResponse},
};

fuzz_target!(|data: &[u8]| {
    let _ = serde_json::from_slice::<TokenResponse>(data);
    let _ = serde_json::from_slice::<ErrorBody>(data);

    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };

    if let Ok((server, _)) = Server::parse(text) {
        assert!(server.origin().starts_with("https://") || server.loopback());
        let (again, key) = Server::parse(server.origin()).unwrap();
        assert_eq!(again.origin(), server.origin());
        assert!(key.is_none());
    }
    if let Ok(key) = EnrollKey::parse(text) {
        assert_eq!(key.expose(), text);
    }
    if let Some(pair) = KeyPair::decode(text) {
        let again = KeyPair::decode(&pair.encode()).unwrap();
        assert_eq!(again.seed(), pair.seed());
    }
    if let Some(mac) = network::parse_mac(text) {
        assert_eq!(network::parse_mac(&network::format_mac(mac)), Some(mac));
    }

    let _ = network::parse_if_inet6(text);
    let _ = network::default_route_interface(text);
    let _ = cloud::parse_aws(text, Some(text));
    let _ = cloud::parse_gcp(text);
    let _ = cloud::parse_azure(text);
});
