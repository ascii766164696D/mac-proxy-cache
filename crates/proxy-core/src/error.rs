use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("TLS/certificate error: {0}")]
    Certificate(#[from] rcgen::Error),

    #[error("Config error: {0}")]
    Config(String),

    #[error("Proxy error: {0}")]
    Proxy(String),
}
