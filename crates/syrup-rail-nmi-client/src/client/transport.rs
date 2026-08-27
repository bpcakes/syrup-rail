use futures_util::StreamExt;
use reqwest::header::AUTHORIZATION;
use reqwest::{StatusCode, Url};
use serde_json::Value;

use crate::{Client, lossless_json::LosslessJsonValue};

use super::form::{NmiFormParams, ensure_form_request_is_bounded};
use super::text::{bounded_detail, bounded_gateway_text};
use super::{MAX_NMI_STANDARD_RESPONSE_BYTES, WireError};

impl Client {
    pub(super) fn endpoint(&self, path: &str) -> Result<Url, WireError> {
        let endpoint = self
            .endpoint
            .url
            .join(path)
            .map_err(|error| WireError::LocalInvalidRequest(error.to_string()))?;
        if endpoint.scheme() != self.endpoint.url.scheme()
            || endpoint.host_str() != self.endpoint.url.host_str()
            || endpoint.port_or_known_default() != self.endpoint.url.port_or_known_default()
        {
            return Err(WireError::LocalInvalidRequest(
                "NMI endpoint escaped configured base URL.".to_owned(),
            ));
        }
        Ok(endpoint)
    }

    pub(super) async fn post_json(
        &self,
        path: &str,
        body: Value,
    ) -> Result<LosslessJsonValue, WireError> {
        let response = self
            .http
            .post(self.endpoint(path)?)
            .header(AUTHORIZATION, self.credentials.private_api_key_header())
            .json(&body)
            .send()
            .await
            .map_err(map_submit_error)?;
        let status = response.status();
        let body =
            read_nmi_response_text(response, status, MAX_NMI_STANDARD_RESPONSE_BYTES).await?;
        if status.is_success() {
            return serde_json::from_str::<LosslessJsonValue>(&body).map_err(|error| {
                WireError::Indeterminate(format!(
                    "NMI returned invalid JSON after accepting a sale request: {error}"
                ))
            });
        }
        Err(gateway_error_from_http_response(status, &body))
    }

    pub(super) async fn post_form_text(
        &self,
        path: &str,
        params: &NmiFormParams<'_>,
    ) -> Result<String, WireError> {
        self.post_form_text_with_limit(path, params, MAX_NMI_STANDARD_RESPONSE_BYTES)
            .await
    }

    pub(super) async fn post_form_text_with_limit(
        &self,
        path: &str,
        params: &NmiFormParams<'_>,
        max_response_bytes: usize,
    ) -> Result<String, WireError> {
        let response = self
            .form_request(path, params)?
            .send()
            .await
            .map_err(map_submit_error)?;
        let status = response.status();
        let body = read_nmi_response_text(response, status, max_response_bytes).await?;
        if status.is_success() {
            return Ok(body);
        }
        Err(gateway_error_from_http_response(status, &body))
    }

    pub(super) fn form_request(
        &self,
        path: &str,
        params: &NmiFormParams<'_>,
    ) -> Result<reqwest::RequestBuilder, WireError> {
        ensure_form_request_is_bounded(params)?;
        // The parameter set borrows credentials, tokens, processor identifiers,
        // and contact data, so it creates no application-owned secret copies.
        // Reqwest necessarily creates one URL-encoded transport body here; its
        // allocator is outside this crate and is not guaranteed to zeroize it.
        Ok(self.http.post(self.endpoint(path)?).form(params))
    }
}

fn map_submit_error(error: reqwest::Error) -> WireError {
    let detail = bounded_detail(&error.to_string());
    if error.is_connect() {
        // Reqwest derives this classification from hyper-util's Connect
        // error, which is produced while acquiring a connection before the
        // request is handed to the HTTP sender. NMI therefore cannot have
        // received this mutation.
        WireError::Unavailable(detail)
    } else {
        // Once a connection exists, a send error cannot prove whether NMI
        // received the complete request. Preserve mutation uncertainty.
        WireError::Indeterminate(detail)
    }
}

fn map_response_read_error(status: StatusCode, error: reqwest::Error) -> WireError {
    if status.is_success() {
        WireError::Indeterminate(bounded_detail(&format!(
            "NMI response body read failed after HTTP {status}: {error}"
        )))
    } else {
        gateway_error_for_http_status(status, format!("NMI returned HTTP {status}"))
    }
}

async fn read_nmi_response_text(
    response: reqwest::Response,
    status: StatusCode,
    max_response_bytes: usize,
) -> Result<String, WireError> {
    if response
        .content_length()
        .is_some_and(|length| length > max_response_bytes as u64)
    {
        return Err(map_response_body_too_large(status, max_response_bytes));
    }

    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| map_response_read_error(status, error))?;
        if body.len().saturating_add(chunk.len()) > max_response_bytes {
            return Err(map_response_body_too_large(status, max_response_bytes));
        }
        body.extend_from_slice(&chunk);
    }
    String::from_utf8(body).map_err(|error| {
        if status.is_success() {
            WireError::Indeterminate(format!(
                "NMI response body was not valid UTF-8 after HTTP {status}: {error}"
            ))
        } else {
            gateway_error_for_http_status(
                status,
                bounded_detail(&format!(
                    "NMI returned HTTP {status} with a non-UTF-8 response body: {error}"
                )),
            )
        }
    })
}

fn map_response_body_too_large(status: StatusCode, max_response_bytes: usize) -> WireError {
    if status.is_success() {
        WireError::Indeterminate(format!(
            "NMI response body exceeded {max_response_bytes} bytes after HTTP {status}"
        ))
    } else {
        gateway_error_for_http_status(
            status,
            format!("NMI returned HTTP {status} with an oversized response body"),
        )
    }
}

pub(super) fn gateway_error_from_http_response(status: StatusCode, body: &str) -> WireError {
    // NMI sometimes returns HTML or plain text error pages. Treat JSON detail
    // parsing as best-effort and fall back to status-based classification.
    let message = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|value| {
            serde_json_string_field(&value, "response_text")
                .or_else(|| serde_json_string_field(&value, "message"))
        })
        .unwrap_or_else(|| format!("NMI returned HTTP {status}"));
    gateway_error_for_http_status(status, bounded_detail(&message))
}

pub(super) fn gateway_error_for_http_status(status: StatusCode, message: String) -> WireError {
    match status {
        StatusCode::BAD_REQUEST | StatusCode::UNPROCESSABLE_ENTITY => {
            WireError::RequestRejected(message)
        }
        StatusCode::UNAUTHORIZED
        | StatusCode::FORBIDDEN
        | StatusCode::NOT_FOUND
        | StatusCode::METHOD_NOT_ALLOWED => WireError::Configuration(message),
        StatusCode::TOO_MANY_REQUESTS => WireError::TransportRateLimited(message),
        // Never add an accepted HTTP response here: `Unavailable` proves that
        // no request reached NMI and authorizes same-attempt, same-order replay.
        // Endpoint/method mismatches also prove non-submission, but they are a
        // durable configuration defect rather than a transient transport
        // outage and are therefore classified above as `Configuration`.
        _ => WireError::Indeterminate(message),
    }
}

fn serde_json_string_field(value: &Value, field: &str) -> Option<String> {
    match value.get(field)? {
        Value::String(value) => bounded_gateway_text(value),
        Value::Number(value) => bounded_gateway_text(&value.to_string()),
        Value::Null | Value::Bool(_) | Value::Array(_) | Value::Object(_) => None,
    }
}
