use thiserror::Error;

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("YAML parse error: {0}")]
    Yaml(#[from] serde_yaml::Error),

    #[error("File watcher error: {0}")]
    Watcher(#[from] notify::Error),

    #[error(
        "server.{field} = {value:?} is not a valid socket address (expected e.g. \"0.0.0.0:69\")"
    )]
    InvalidBindAddr { field: String, value: String },

    #[error("host {host:?} has an invalid MAC address {mac:?} (expected aa:bb:cc:dd:ee:ff)")]
    InvalidMac { host: String, mac: String },

    #[error("duplicate host MAC {0:?} (each host MAC must be unique)")]
    DuplicateMac(String),

    #[error("duplicate host name {0:?} (each host name must be unique)")]
    DuplicateName(String),

    #[error("server.{0} must not be empty")]
    EmptyBootloader(String),

    #[error("server.api_token is set but empty; unset it to disable auth or provide a real secret")]
    EmptyApiToken,

    #[error("server.max_artifact_bytes is 0 (rejects all artifacts); unset it for unbounded")]
    ZeroArtifactCap,

    #[error(
        "server.advertised_host = {value:?} must be a non-empty host[:port] without whitespace"
    )]
    InvalidAdvertisedHost { value: String },

    #[error(
        "server.allowed_hosts entry {value:?} must be a non-empty hostname/IP without whitespace"
    )]
    InvalidAllowedHost { value: String },

    #[error("a host has an empty name")]
    EmptyHostName,

    #[error("host name {value:?} contains a newline or carriage return (\\n or \\r)")]
    InvalidHostName { value: String },

    #[error("host {host:?} cmdline contains a newline or carriage return (\\n or \\r)")]
    InvalidHostCmdline { host: String, value: String },
}
