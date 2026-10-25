use thiserror::Error;

#[derive(Debug, Error)]
pub enum AppError {
    #[error("missing required environment variable: {0}")]
    MissingEnv(&'static str),

    #[error("invalid environment variable {name}: {reason}")]
    InvalidEnv { name: &'static str, reason: String },

    #[error("s3 upload error: {0}")]
    S3Upload(String),
}
