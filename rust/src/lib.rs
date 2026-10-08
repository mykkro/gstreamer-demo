//! Shared bits of the demo: the JSON control protocol (identical to the Python version,
//! so Rust and Python producers/receivers can be mixed freely).
//!
//! Protocol: newline-delimited JSON over TCP.
//!   -> {"cmd": "list"}    <- {"ok": true, "source": ..., "channels": [...]}
//!   -> {"cmd": "stats"}   <- {"ok": true, "clients": {"720p30": 2, ...}}

use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

pub const DEFAULT_CONTROL_PORT: u16 = 5000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Channel {
    pub id: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub port: u16,
    pub codec: String,
    pub container: String,
    pub transport: String,
    pub encoder: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelList {
    pub ok: bool,
    pub source: String,
    pub channels: Vec<Channel>,
}

/// Send one JSON command to the producer's control port and return the raw reply.
pub fn request(host: &str, port: u16, cmd: &str) -> Result<serde_json::Value> {
    let addr = std::net::ToSocketAddrs::to_socket_addrs(&(host, port))?
        .next()
        .context("cannot resolve producer address")?;
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(3))?;
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    stream.write_all(format!("{}\n", serde_json::json!({ "cmd": cmd })).as_bytes())?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    if line.is_empty() {
        bail!("producer closed control connection without reply");
    }
    Ok(serde_json::from_str(&line)?)
}

pub fn list_channels(host: &str, port: u16) -> Result<ChannelList> {
    Ok(serde_json::from_value(request(host, port, "list")?)?)
}

pub fn has_element(name: &str) -> bool {
    gst::ElementFactory::find(name).is_some()
}
