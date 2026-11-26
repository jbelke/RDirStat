//! The `/v1` routes over hyper. [`route`] maps a method, path and body to a
//! status and JSON; [`serve_connection`] is the only part that knows the
//! transport is a Unix socket.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use http_body_util::{BodyExt as _, Full, Limited};
use hyper::body::{Bytes, Incoming};
use hyper::{Method, Request, Response, StatusCode};
use rdirstat_jobs::{ApiError, EVENTS_DEFAULT_WAIT_SECS, EVENTS_MAX_LIMIT, EVENTS_MAX_WAIT_SECS, NewJob};
use serde::Serialize;

use super::{Action, ApiFailure, Service};

/// Largest request body accepted.
const MAX_BODY: usize = 1 << 20;

/// Serves one connection until the client closes it.
pub(crate) async fn serve_connection(service: Arc<Service>, stream: tokio::net::UnixStream) {
    let io = hyper_util::rt::TokioIo::new(stream);
    let handler = hyper::service::service_fn(move |request| handle(Arc::clone(&service), request));
    let _ignored = hyper::server::conn::http1::Builder::new()
        .serve_connection(io, handler)
        .await;
}

async fn handle(service: Arc<Service>, request: Request<Incoming>) -> Result<Response<Full<Bytes>>, Infallible> {
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let query = request.uri().query().unwrap_or_default().to_owned();
    let body = match Limited::new(request.into_body(), MAX_BODY).collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(error) => {
            return Ok(failure(&ApiFailure::new(
                400,
                format!("cannot read the request body: {error}"),
            )));
        }
    };
    Ok(match route(&service, &method, &path, &query, &body).await {
        Ok(response) => response,
        Err(error) => failure(&error),
    })
}

type Routed = Result<Response<Full<Bytes>>, ApiFailure>;

/// Dispatches one request. Transport-free: a TCP listener would call this
/// with the same arguments.
pub(crate) async fn route(service: &Arc<Service>, method: &Method, path: &str, query: &str, body: &[u8]) -> Routed {
    let Some(rest) = path.strip_prefix(rdirstat_jobs::API_PREFIX) else {
        return Err(ApiFailure::new(404, format!("no route {path}; the API is under /v1")));
    };
    let parts: Vec<&str> = rest.split('/').filter(|part| !part.is_empty()).collect();
    match (method, parts.as_slice()) {
        (&Method::GET, ["health"]) => json(200, &service.health()),
        (&Method::POST, ["shutdown"]) => {
            service.request_shutdown();
            json(202, &service.health())
        }
        (&Method::GET, ["jobs"]) => json(200, &service.list()),
        (&Method::POST, ["jobs"]) => {
            let new: NewJob = serde_json::from_slice(body)
                .map_err(|error| ApiFailure::new(400, format!("the body is not a job: {error}")))?;
            json(201, &service.create(new)?)
        }
        (&Method::GET, ["jobs", id]) => json(200, &service.get(id)?),
        (&Method::POST, ["jobs", id, action]) => {
            let action = match *action {
                "pause" => Action::Pause,
                "resume" => Action::Resume,
                "cancel" => Action::Cancel,
                other => return Err(ApiFailure::new(404, format!("no action {other:?}"))),
            };
            json(200, &service.act(id, action)?)
        }
        (&Method::GET, ["jobs", id, "events"]) => events(service, id, query).await,
        (_, ["health" | "shutdown" | "jobs"] | ["jobs", _] | ["jobs", _, _]) => {
            Err(ApiFailure::new(405, format!("{method} is not allowed on {path}")))
        }
        _ => Err(ApiFailure::new(404, format!("no route {path}"))),
    }
}

/// `GET /v1/jobs/{id}/events?after=&wait=&limit=`: a long-poll.
async fn events(service: &Arc<Service>, id: &str, query: &str) -> Routed {
    let after = param(query, "after")?.unwrap_or(0);
    let wait = param(query, "wait")?
        .unwrap_or(EVENTS_DEFAULT_WAIT_SECS)
        .min(EVENTS_MAX_WAIT_SECS);
    let limit = param(query, "limit")?
        .map_or(EVENTS_MAX_LIMIT, |limit| {
            usize::try_from(limit).unwrap_or(EVENTS_MAX_LIMIT)
        })
        .clamp(1, EVENTS_MAX_LIMIT);
    let mut source = service.events_source(id)?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(wait);
    loop {
        source.seq.borrow_and_update();
        let page = source.page(after, limit)?;
        let stopping = *source.stopping.borrow();
        if !page.events.is_empty() || page.finished || stopping || tokio::time::Instant::now() >= deadline {
            return json(200, &page);
        }
        tokio::select! {
            _ = source.seq.changed() => {}
            _ = source.stopping.changed() => {}
            () = tokio::time::sleep_until(deadline) => {}
        }
    }
}

/// A non-negative integer query parameter.
fn param(query: &str, name: &str) -> Result<Option<u64>, ApiFailure> {
    for pair in query.split('&') {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        if key == name {
            return value
                .parse()
                .map(Some)
                .map_err(|_| ApiFailure::new(400, format!("{name}={value:?} is not a non-negative integer")));
        }
    }
    Ok(None)
}

fn json<T: Serialize>(status: u16, value: &T) -> Routed {
    let mut body = serde_json::to_vec(value).map_err(|error| ApiFailure::new(500, error.to_string()))?;
    body.push(b'\n');
    Ok(respond(status, body))
}

fn failure(error: &ApiFailure) -> Response<Full<Bytes>> {
    let mut body = serde_json::to_vec(&ApiError {
        error: error.message.clone(),
    })
    .unwrap_or_default();
    body.push(b'\n');
    respond(error.status, body)
}

fn respond(status: u16, body: Vec<u8>) -> Response<Full<Bytes>> {
    let mut response = Response::new(Full::new(Bytes::from(body)));
    *response.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    response.headers_mut().insert(
        hyper::header::CONTENT_TYPE,
        hyper::header::HeaderValue::from_static("application/json"),
    );
    response
}
