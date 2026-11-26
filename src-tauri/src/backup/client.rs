//! An async client of a drive's backup service: HTTP/1.1 + JSON over its
//! Unix socket, one request per connection, every call bounded by a timeout.
//! The routes and bodies are `rdirstat_jobs`'s; this module knows only the
//! transport.

use std::path::Path;
use std::time::Duration;

use http_body_util::{BodyExt as _, Full};
use hyper::body::Bytes;
use hyper::{Method, Request};
use rdirstat_jobs::ApiError;
use serde::de::DeserializeOwned;

/// Why a call did not return a value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CallError {
    /// Nothing answers on the socket: no service, or a dead one's socket.
    Unreachable(String),
    /// The service did not answer in time.
    TimedOut,
    /// The service answered with an error status.
    Api { status: u16, message: String },
    /// The answer was not HTTP, or not the JSON expected.
    Protocol(String),
}

impl CallError {
    /// No service is there to ask (as opposed to one that refused).
    pub(crate) fn is_unreachable(&self) -> bool {
        matches!(self, Self::Unreachable(_))
    }
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreachable(error) => write!(f, "no backup service answers: {error}"),
            Self::TimedOut => f.write_str("the backup service did not answer in time"),
            Self::Api { status, message } => write!(f, "{message} (HTTP {status})"),
            Self::Protocol(error) => write!(f, "bad answer from the backup service: {error}"),
        }
    }
}

impl std::error::Error for CallError {}

fn protocol(error: impl std::fmt::Display) -> CallError {
    CallError::Protocol(error.to_string())
}

/// Sends one request to the service at `socket` and decodes a `2xx` JSON
/// answer as `T`.
///
/// # Errors
///
/// See [`CallError`].
pub(crate) async fn call<T: DeserializeOwned>(
    socket: &Path,
    method: Method,
    path: &str,
    body: Option<Vec<u8>>,
    timeout: Duration,
) -> Result<T, CallError> {
    tokio::time::timeout(timeout, exchange(socket, method, path, body))
        .await
        .map_err(|_| CallError::TimedOut)?
}

async fn exchange<T: DeserializeOwned>(
    socket: &Path,
    method: Method,
    path: &str,
    body: Option<Vec<u8>>,
) -> Result<T, CallError> {
    let stream = tokio::net::UnixStream::connect(socket)
        .await
        .map_err(|error| CallError::Unreachable(format!("{}: {error}", socket.display())))?;
    let (mut sender, connection) = hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(stream))
        .await
        .map_err(protocol)?;
    let driver = tokio::spawn(async move {
        // The connection ends when the answer is read or the call is
        // dropped; either way there is nothing to report from here.
        drop(connection.await);
    });
    let bytes = body.unwrap_or_default();
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header(hyper::header::HOST, "localhost");
    if !bytes.is_empty() {
        request = request.header(hyper::header::CONTENT_TYPE, "application/json");
    }
    let request = request.body(Full::new(Bytes::from(bytes))).map_err(protocol)?;
    let response = sender.send_request(request).await.map_err(protocol)?;
    let status = response.status().as_u16();
    let body = response.into_body().collect().await.map_err(protocol)?.to_bytes();
    driver.abort();
    if !(200..300).contains(&status) {
        let message = serde_json::from_slice::<ApiError>(&body)
            .map_or_else(|_| String::from_utf8_lossy(&body).into_owned(), |error| error.error);
        return Err(CallError::Api { status, message });
    }
    serde_json::from_slice(&body).map_err(protocol)
}

#[cfg(test)]
pub(crate) mod tests {
    #![allow(clippy::expect_used, reason = "test code: a failed fixture must abort loudly")]

    use std::io::{Read as _, Write as _};
    use std::os::unix::net::UnixListener;

    use super::*;

    /// A one-shot fake service: answers each of `answers`, in order, to one
    /// connection each, and hands back the request lines it saw.
    pub(crate) fn fake_service(socket: &Path, answers: Vec<(u16, String)>) -> std::thread::JoinHandle<Vec<String>> {
        let listener = UnixListener::bind(socket).expect("bind");
        std::thread::spawn(move || {
            let mut seen = Vec::new();
            for (status, body) in answers {
                let (mut stream, _) = listener.accept().expect("accept");
                let mut request = Vec::new();
                let mut buf = [0_u8; 4096];
                while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                    let read = stream.read(&mut buf).expect("read");
                    if read == 0 {
                        break;
                    }
                    request.extend_from_slice(&buf[..read]);
                }
                let text = String::from_utf8_lossy(&request).into_owned();
                seen.push(text.lines().next().unwrap_or_default().to_owned());
                let answer = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(answer.as_bytes()).expect("write");
            }
            seen
        })
    }

    /// A short socket path: `$TMPDIR` on macOS is too long for `sun_path`.
    pub(crate) fn short_dir() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("rdb")
            .tempdir_in("/tmp")
            .expect("tempdir")
    }

    #[test]
    fn decodes_a_2xx_answer_and_turns_an_error_status_into_its_message() {
        let dir = short_dir();
        let socket = dir.path().join("s.sock");
        let server = fake_service(
            &socket,
            vec![
                (200, r#"{"error":"not an error, a payload"}"#.to_owned()),
                (409, r#"{"error":"job \"x\" exists"}"#.to_owned()),
            ],
        );
        let ok: ApiError =
            tauri::async_runtime::block_on(call(&socket, Method::GET, "/v1/health", None, Duration::from_secs(5)))
                .expect("2xx decodes");
        assert_eq!(ok.error, "not an error, a payload");
        let refused = tauri::async_runtime::block_on(call::<ApiError>(
            &socket,
            Method::POST,
            "/v1/jobs",
            Some(b"{}".to_vec()),
            Duration::from_secs(5),
        ))
        .expect_err("409 is an error");
        assert_eq!(
            refused,
            CallError::Api {
                status: 409,
                message: r#"job "x" exists"#.to_owned()
            }
        );
        let seen = server.join().expect("server");
        assert_eq!(seen, vec!["GET /v1/health HTTP/1.1", "POST /v1/jobs HTTP/1.1"]);
    }

    #[test]
    fn a_missing_or_dead_socket_is_unreachable() {
        let dir = short_dir();
        let missing = dir.path().join("none.sock");
        let error = tauri::async_runtime::block_on(call::<ApiError>(
            &missing,
            Method::GET,
            "/v1/health",
            None,
            Duration::from_secs(5),
        ))
        .expect_err("nothing listens");
        assert!(error.is_unreachable(), "{error}");

        let dead = dir.path().join("dead.sock");
        drop(UnixListener::bind(&dead).expect("bind"));
        let error = tauri::async_runtime::block_on(call::<ApiError>(
            &dead,
            Method::GET,
            "/v1/health",
            None,
            Duration::from_secs(5),
        ))
        .expect_err("a dead socket refuses");
        assert!(error.is_unreachable(), "{error}");
    }
}
