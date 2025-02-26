use crate::adaptor::ConduitRequest;
use crate::error::ServiceError;
use crate::file_stream::FileStream;
use crate::{AxumResponse, ConduitResponse};

use std::error::Error;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::{Body, HttpBody};
use axum::extract::{ConnectInfo, Extension};
use axum::handler::Handler as AxumHandler;
use axum::response::IntoResponse;
use conduit::{Handler, RequestExt, StartInstant};
use conduit_router::RoutePattern;
use http::header::CONTENT_LENGTH;
use http::StatusCode;
use hyper::{Request, Response};
use sentry_core::Hub;
use tracing::{error, warn};

/// The maximum size allowed in the `Content-Length` header
///
/// Chunked requests may grow to be larger over time if that much data is actually sent.
/// See the usage section of the README if you plan to use this server in production.
const MAX_CONTENT_LENGTH: u64 = 128 * 1024 * 1024; // 128 MB

pub trait ConduitFallback {
    fn conduit_fallback(self, handler: impl Handler) -> Self;
}

impl ConduitFallback for axum::Router {
    fn conduit_fallback(self, handler: impl Handler) -> Self {
        let handler: Arc<dyn Handler> = Arc::new(handler);
        self.fallback(fallback_to_conduit.layer(Extension(handler)))
    }
}

async fn fallback_to_conduit(
    handler: Extension<Arc<dyn Handler>>,
    ConnectInfo(remote_addr): ConnectInfo<SocketAddr>,
    request: Request<Body>,
) -> Result<AxumResponse, ServiceError> {
    if let Err(response) = check_content_length(&request) {
        return Ok(response);
    }

    let (parts, body) = request.into_parts();
    let now = StartInstant::now();

    let hub = Hub::current();

    let full_body = hyper::body::to_bytes(body).await?;
    let request = Request::from_parts(parts, full_body);

    let handler = handler.clone();
    tokio::task::spawn_blocking(move || {
        Hub::run(hub, || {
            let mut request = ConduitRequest::new(request, remote_addr, now);
            handler
                .call(&mut request)
                .map(|response| conduit_into_axum(response, request))
                .unwrap_or_else(|e| server_error_response(&*e))
        })
    })
    .await
    .map_err(Into::into)
}

/// Turns a `ConduitResponse` into a `AxumResponse`
fn conduit_into_axum(mut response: ConduitResponse, mut request: ConduitRequest) -> AxumResponse {
    use conduit::Body::*;

    if let Some(pattern) = request.mut_extensions().remove::<RoutePattern>() {
        response.extensions_mut().insert(pattern);
    }

    let (parts, body) = response.into_parts();
    match body {
        Static(slice) => Response::from_parts(parts, axum::body::Body::from(slice)).into_response(),
        Owned(vec) => Response::from_parts(parts, axum::body::Body::from(vec)).into_response(),
        File(file) => Response::from_parts(parts, FileStream::from_std(file).into_streamed_body())
            .into_response(),
    }
}

impl IntoResponse for ServiceError {
    fn into_response(self) -> AxumResponse {
        server_error_response(&self)
    }
}

/// Logs an error message and returns a generic status 500 response
fn server_error_response<E: Error + ?Sized>(error: &E) -> AxumResponse {
    error!(%error, "Internal Server Error");

    sentry_core::capture_error(error);

    let body = hyper::Body::from("Internal Server Error");
    Response::builder()
        .status(StatusCode::INTERNAL_SERVER_ERROR)
        .body(body)
        .expect("Unexpected invalid header")
        .into_response()
}

/// Check for `Content-Length` values that are invalid or too large
///
/// If a `Content-Length` is provided then `hyper::body::to_bytes()` may try to allocate a buffer
/// of this size upfront, leading to a process abort and denial of service to other clients.
///
/// This only checks for requests that claim to be too large. If the request is chunked then it
/// is possible to allocate larger chunks of memory over time, by actually sending large volumes of
/// data. Request sizes must be limited higher in the stack to protect against this type of attack.
fn check_content_length(request: &Request<Body>) -> Result<(), AxumResponse> {
    fn bad_request(message: &str) -> AxumResponse {
        warn!("Bad request: Content-Length {}", message);

        Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .body(Body::empty())
            .expect("Unexpected invalid header")
            .into_response()
    }

    if let Some(content_length) = request.headers().get(CONTENT_LENGTH) {
        let content_length = match content_length.to_str() {
            Ok(some) => some,
            Err(_) => return Err(bad_request("not ASCII")),
        };

        let content_length = match content_length.parse::<u64>() {
            Ok(some) => some,
            Err(_) => return Err(bad_request("not a u64")),
        };

        if content_length > MAX_CONTENT_LENGTH {
            return Err(bad_request("too large"));
        }
    }

    // A duplicate check, aligning with the specific impl of `hyper::body::to_bytes`
    // (at the time of this writing)
    if request.size_hint().lower() > MAX_CONTENT_LENGTH {
        return Err(bad_request("size_hint().lower() too large"));
    }

    Ok(())
}
