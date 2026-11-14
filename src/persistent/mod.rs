//! PVC上の完成動画とmanifestを正本にし、単一uploaderがS3へ転送する。
use crate::{config::AppConfig, key::sanitize_camera_id};
use anyhow::{Context, Result, bail};
use axum::{Router, extract::State, http::StatusCode, routing::get};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    env,
    fs::{self, File},
    io::Read,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use time::{OffsetDateTime, UtcOffset, format_description::well_known::Rfc3339};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};

mod recorder;
mod spool;
mod uploader;
use recorder::*;
use spool::*;
use uploader::*;

pub async fn dispatch() -> Result<()> {
    let args: Vec<_> = env::args().skip(1).collect();
    if args.len() != 1 {
        bail!("Usage: home-camera --record|--upload");
    }
    match args[0].as_str() {
        "--record" => recorder().await?,
        "--upload" => uploader().await?,
        _ => bail!("Usage: home-camera --record|--upload"),
    }
    Ok(())
}

#[cfg(test)]
mod tests;
