use std::path::Path;

use anyhow::{anyhow, Context, Result};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

/// Send one JSON request over the admin socket and return the parsed response.
pub(crate) async fn call_admin(sock: &Path, request: Value) -> Result<Value> {
    let mut stream = UnixStream::connect(sock)
        .await
        .with_context(|| format!("connect to admin socket {:?}", sock))?;

    let mut line = serde_json::to_string(&request)?;
    line.push('\n');
    stream.write_all(line.as_bytes()).await?;

    let (reader, _) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();
    let resp_line = lines
        .next_line()
        .await?
        .ok_or_else(|| anyhow!("admin socket closed without response"))?;

    serde_json::from_str(&resp_line).context("parse admin response")
}

/// Return `Ok(resp)` when `ok == true`, or convert the `error` field to `Err`.
pub(crate) fn check_ok(resp: Value) -> Result<Value> {
    if resp["ok"].as_bool().unwrap_or(false) {
        Ok(resp)
    } else {
        Err(anyhow!(
            "{}",
            resp["error"].as_str().unwrap_or("unknown error")
        ))
    }
}
