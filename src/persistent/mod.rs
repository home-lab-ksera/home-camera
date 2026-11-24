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
    const USAGE: &str = "Usage: home-camera --record|--upload|--analyze [--debug] FILE.mp4";
    let args: Vec<_> = env::args().skip(1).collect();
    match args.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        ["--record"] => recorder().await?,
        ["--upload"] => uploader().await?,
        // 調整用。録画1本を解析し、結果を JSON で標準出力に出す。
        ["--analyze", file] => analyze(file, false)?,
        ["--analyze", "--debug", file] => analyze(file, true)?,
        _ => bail!(USAGE),
    }
    Ok(())
}

fn analyze(file: &str, debug: bool) -> Result<()> {
    let params = crate::motion::Params::from_env()?;
    let report = crate::motion::analyze_file(Path::new(file), &params, debug)?;
    println!("{}", serde_json::to_string(&report)?);
    Ok(())
}

#[cfg(test)]
mod tests;
