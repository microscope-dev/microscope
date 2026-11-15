



use std::collections::HashMap;
use std::fmt;

#[derive(Debug)]
enum ConfigError {
    Missing(String),
    BadNumber { key: String, value: String },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Missing(key) => write!(f, "missing key {key}"),
            ConfigError::BadNumber { key, value } => write!(f, "{key} = {value:?} is not a number"),
        }
    }
}

struct Config {
    entries: HashMap<String, String>,
}

impl Config {
    fn parse(text: &str) -> Config {
        let entries = text
            .lines()
            .filter_map(|line| line.split_once('='))
            .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
            .collect();
        Config { entries }
    }

    fn port(&self) -> Result<u16, ConfigError> {
        let value = self
            .entries
            .get("port")
            .ok_or_else(|| ConfigError::Missing("port".to_string()))?;
        value.parse().map_err(|_| ConfigError::BadNumber {
            key: "port".to_string(),
            value: value.clone(),
        })
    }
}

fn main() {
    let config = Config::parse("host = localhost\nport = 80a0\n");
    let port = config.port();
    println!("port result computed");
    let port = port.unwrap();
    println!("listening on {port}");
}
