//! A blocking client for the service's socket: one request per connection
//! (`Connection: close`), JSON in and out. Enough for `backup jobs`,
//! `backup submit`, `backup control`, `serve --detach` and the tests.

use std::io::{Read as _, Write as _};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use rdirstat_jobs::ApiError;
use serde::de::DeserializeOwned;

/// Why a call did not return a value.
#[derive(Debug)]
pub(crate) enum CallError {
    /// Nothing answers on the socket (no service, or a stale socket).
    Unreachable(std::io::Error),
    /// The service answered with an error status.
    Api { status: u16, message: String },
    /// The answer was not HTTP or not the JSON expected.
    Protocol(anyhow::Error),
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreachable(error) => write!(f, "no backup service answers: {error}"),
            Self::Api { status, message } => write!(f, "{message} (HTTP {status})"),
            Self::Protocol(error) => write!(f, "bad answer from the backup service: {error:#}"),
        }
    }
}

impl std::error::Error for CallError {}

/// Sends one request and decodes a `2xx` JSON answer as `T`.
///
/// # Errors
///
/// See [`CallError`].
pub(crate) fn call<T: DeserializeOwned>(
    socket: &Path,
    method: &str,
    path: &str,
    body: Option<&[u8]>,
    timeout: Duration,
) -> std::result::Result<T, CallError> {
    let mut stream = UnixStream::connect(socket).map_err(CallError::Unreachable)?;
    let (status, bytes) = exchange(&mut stream, method, path, body, timeout).map_err(CallError::Protocol)?;
    if !(200..300).contains(&status) {
        let message = serde_json::from_slice::<ApiError>(&bytes)
            .map_or_else(|_| String::from_utf8_lossy(&bytes).into_owned(), |error| error.error);
        return Err(CallError::Api { status, message });
    }
    serde_json::from_slice(&bytes).map_err(|error| CallError::Protocol(error.into()))
}

fn exchange(
    stream: &mut UnixStream,
    method: &str,
    path: &str,
    body: Option<&[u8]>,
    timeout: Duration,
) -> Result<(u16, Vec<u8>)> {
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    let body = body.unwrap_or_default();
    let mut request = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Length: {}\r\n",
        body.len()
    );
    if !body.is_empty() {
        request.push_str("Content-Type: application/json\r\n");
    }
    request.push_str("\r\n");
    stream.write_all(request.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()?;
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).context("reading the answer")?;
    parse_response(&raw)
}

/// Splits an HTTP/1.1 response into its status and (de-chunked) body.
fn parse_response(raw: &[u8]) -> Result<(u16, Vec<u8>)> {
    let Some(split) = raw.windows(4).position(|window| window == b"\r\n\r\n") else {
        bail!("no end of headers in {} bytes", raw.len());
    };
    let head = std::str::from_utf8(&raw[..split]).context("headers are not UTF-8")?;
    let body = &raw[split + 4..];
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or_default();
    let status: u16 = status_line
        .split(' ')
        .nth(1)
        .and_then(|code| code.parse().ok())
        .with_context(|| format!("bad status line {status_line:?}"))?;
    let mut chunked = false;
    let mut length = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("transfer-encoding") && value.eq_ignore_ascii_case("chunked") {
            chunked = true;
        } else if name.eq_ignore_ascii_case("content-length") {
            length = value.parse::<usize>().ok();
        }
    }
    let body = if chunked {
        dechunk(body)?
    } else if let Some(length) = length {
        body.get(..length)
            .context("the body is shorter than its Content-Length")?
            .to_vec()
    } else {
        body.to_vec()
    };
    Ok((status, body))
}

fn dechunk(mut body: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let Some(end) = body.windows(2).position(|window| window == b"\r\n") else {
            bail!("a chunk has no size line");
        };
        let size_text = std::str::from_utf8(&body[..end])?;
        let size = usize::from_str_radix(size_text.split(';').next().unwrap_or_default().trim(), 16)
            .with_context(|| format!("bad chunk size {size_text:?}"))?;
        body = &body[end + 2..];
        if size == 0 {
            return Ok(out);
        }
        out.extend_from_slice(body.get(..size).context("a chunk is cut short")?);
        body = body.get(size + 2..).context("a chunk is cut short")?;
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "test code")]

    use super::*;

    #[test]
    fn parses_length_and_chunked_bodies() {
        let (status, body) = parse_response(b"HTTP/1.1 201 Created\r\ncontent-length: 2\r\n\r\n{}").unwrap();
        assert_eq!((status, body.as_slice()), (201, b"{}".as_slice()));
        let (status, body) =
            parse_response(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n")
                .unwrap();
        assert_eq!((status, body.as_slice()), (200, b"abcde".as_slice()));
    }
}
