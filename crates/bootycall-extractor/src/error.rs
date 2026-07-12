use std::path::PathBuf;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ExtractorError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("ISO reader error: {0}")]
    Iso(String),

    #[error("Disk reader error: {0}")]
    Disk(String),

    #[error("Image file not found: {0}")]
    ImageNotFound(PathBuf),

    #[error("Kernel file not found inside the image")]
    KernelNotFound,

    #[error("Initrd file not found inside the image")]
    InitrdNotFound,

    #[error("Extracted artifact exceeds the configured max_artifact_bytes limit of {limit} bytes")]
    ArtifactTooLarge { limit: u64 },

    #[error("Maximum directory depth ({0}) exceeded")]
    MaxDepthExceeded(usize),
}
