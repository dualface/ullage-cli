//! Loopback OAuth callback receiver for Devin's browser sign-in.
//!
//! The provider binds an ephemeral `127.0.0.1` port during `start_auth` and a
//! background task accepts connections until a request carrying the expected
//! `state` arrives. The browser sees a small HTML page; the outcome is handed
//! to `complete_auth` through a shared slot.

use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// A request larger than this cannot be a real OAuth redirect.
const MAX_REQUEST_BYTES: usize = 16 * 1024;

/// What the listener observed once a request with the right `state` arrived.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CallbackOutcome {
    /// `code` and `state` were both present and the state matched.
    Code(String),
    /// The authorization server bounced back an `error` parameter.
    Denied(String),
}

/// Receives one OAuth redirect on an ephemeral loopback port.
///
/// `peek` reports the captured outcome to whoever completes the flow; it
/// stays in the slot until the receiver is dropped, so a transiently failed
/// exchange can retry the same authorization code. Dropping the receiver
/// aborts the accept task, so a superseded flow releases its port
/// immediately.
pub struct LoopbackCallback {
    redirect_uri: String,
    received: Arc<Mutex<Option<CallbackOutcome>>>,
    accept_task: tokio::task::JoinHandle<()>,
}

impl LoopbackCallback {
    /// Binds `127.0.0.1:0` and starts accepting. Requires a Tokio runtime;
    /// callers without one get `BindError` and can fall back to the paste
    /// path.
    pub fn bind(expected_state: &str, expires_at: DateTime<Utc>) -> Result<Self, BindError> {
        // `tokio::spawn` would panic outside a runtime; check first so the
        // caller sees a recoverable BindError instead.
        tokio::runtime::Handle::try_current()
            .map_err(|_| BindError(std::io::Error::other("no Tokio runtime")))?;
        let std_listener = std::net::TcpListener::bind(("127.0.0.1", 0))?;
        std_listener.set_nonblocking(true)?;
        let listener = tokio::net::TcpListener::from_std(std_listener)?;
        let port = listener.local_addr()?.port();
        let received = Arc::new(Mutex::new(None));
        let accept_task = tokio::spawn(accept_loop(
            listener,
            expected_state.to_owned(),
            expires_at,
            Arc::clone(&received),
        ));
        Ok(Self {
            redirect_uri: format!("http://127.0.0.1:{port}/callback"),
            received,
            accept_task,
        })
    }

    /// The `redirect_uri` the authorization URL must carry: the listener's
    /// own address on the `/callback` path.
    pub fn redirect_uri(&self) -> &str {
        &self.redirect_uri
    }

    /// The captured outcome, if one landed. It stays in the slot until the
    /// receiver is dropped, so an exchange that fails transiently can retry
    /// with the same authorization code on the next poll. A request with a
    /// mismatched `state` never lands here: it only earns the failure page
    /// and the listener keeps waiting for the real redirect.
    pub fn peek(&self) -> Option<CallbackOutcome> {
        self.received
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

impl Drop for LoopbackCallback {
    fn drop(&mut self) {
        self.accept_task.abort();
    }
}

/// Binding or registering the listener failed. The inner error is only ever
/// logged by the caller, never returned to the OAuth server.
#[derive(Debug)]
pub struct BindError(std::io::Error);

impl std::fmt::Display for BindError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "loopback callback listener unavailable: {}",
            self.0
        )
    }
}

impl std::error::Error for BindError {}

impl From<std::io::Error> for BindError {
    fn from(error: std::io::Error) -> Self {
        Self(error)
    }
}

/// Accepts connections until the flow expires or a request carrying the
/// expected `state` stores its outcome. Other requests still get a proper
/// HTTP answer so a stray browser hit (favicon, wrong path) cannot kill the
/// listener early.
async fn accept_loop(
    listener: tokio::net::TcpListener,
    expected_state: String,
    expires_at: DateTime<Utc>,
    received: Arc<Mutex<Option<CallbackOutcome>>>,
) {
    loop {
        let remaining = (expires_at - Utc::now()).to_std();
        let Ok(remaining) = remaining else {
            return;
        };
        let accepted = tokio::time::timeout(remaining, listener.accept()).await;
        let Ok(Ok((mut stream, _))) = accepted else {
            return;
        };
        if let Some(outcome) = handle_connection(&mut stream, &expected_state).await {
            *received
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(outcome);
            return;
        }
    }
}

/// Reads one request, answers it, and returns the outcome when it carried
/// the expected `state`. Wrong paths and foreign state get an answer too,
/// but no outcome, so the listener survives them.
async fn handle_connection(
    stream: &mut tokio::net::TcpStream,
    expected_state: &str,
) -> Option<CallbackOutcome> {
    let mut buf = Vec::with_capacity(2048);
    let mut chunk = [0_u8; 1024];
    loop {
        if buf.windows(4).any(|window| window == b"\r\n\r\n")
            || buf.windows(2).any(|window| window == b"\n\n")
        {
            break;
        }
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                buf.extend_from_slice(&chunk[..read]);
                if buf.len() > MAX_REQUEST_BYTES {
                    break;
                }
            }
        }
    }
    let request = String::from_utf8_lossy(&buf);
    let target = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("/");
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    if path != "/callback" {
        write_page(stream, 404, "Not Found", "Unknown path.").await;
        return None;
    }
    let mut code = None;
    let mut state = None;
    let mut error = None;
    for pair in query.split('&').filter(|pair| !pair.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        match key {
            "code" => code = Some(form_decode(value)),
            "state" => state = Some(form_decode(value)),
            "error" => error = Some(form_decode(value)),
            _ => {}
        }
    }
    if state.as_deref() != Some(expected_state) {
        write_page(
            stream,
            400,
            "Authorization failed",
            "State mismatch. For security reasons the request was rejected.",
        )
        .await;
        return None;
    }
    if let Some(error) = error {
        write_page(
            stream,
            200,
            "Authorization failed",
            "Devin denied the sign-in. You can close this tab and return to Ullage.",
        )
        .await;
        return Some(CallbackOutcome::Denied(error));
    }
    match code {
        Some(code) => {
            write_page(
                stream,
                200,
                "Signed in",
                "Devin sign-in complete. You can close this tab and return to Ullage.",
            )
            .await;
            Some(CallbackOutcome::Code(code))
        }
        None => {
            write_page(
                stream,
                400,
                "Authorization incomplete",
                "No authorization code was received. You can close this tab.",
            )
            .await;
            // A valid state with neither code nor error is a dead end: the
            // redirect will not be retried, so the flow cannot wait longer.
            Some(CallbackOutcome::Denied(
                "the Devin callback carried no authorization code".into(),
            ))
        }
    }
}

/// The one thing a browser sees back. Self-contained HTML so the tab never
/// has to load anything else.
async fn write_page(stream: &mut tokio::net::TcpStream, status: u16, heading: &str, message: &str) {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        _ => "Not Found",
    };
    let html = format!(
        "<!DOCTYPE html><html><head><meta charset=\"utf-8\"><title>{heading}</title></head>\
         <body><h1>{heading}</h1><p>{message}</p></body></html>"
    );
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{html}",
        html.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
}

/// `application/x-www-form-urlencoded` decoding for the few query keys an
/// OAuth callback can carry. `+` means space; `%XX` is one byte, decoded as
/// UTF-8 lossily because a malformed code fails at the exchange anyway.
fn form_decode(value: &str) -> String {
    let mut bytes = Vec::with_capacity(value.len());
    let mut rest = value.as_bytes();
    while let Some(&byte) = rest.first() {
        match byte {
            b'+' => {
                bytes.push(b' ');
                rest = &rest[1..];
            }
            b'%' if rest.len() >= 3 => {
                let hex = |b: u8| (b as char).to_digit(16);
                match (hex(rest[1]), hex(rest[2])) {
                    (Some(high), Some(low)) => {
                        bytes.push((high * 16 + low) as u8);
                        rest = &rest[3..];
                    }
                    // A malformed escape keeps its `%` and parsing continues
                    // after it, so `%41` still decodes.
                    _ => {
                        bytes.push(byte);
                        rest = &rest[1..];
                    }
                }
            }
            _ => {
                bytes.push(byte);
                rest = &rest[1..];
            }
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_form_urlencoded_values() {
        assert_eq!(form_decode("a+b%2Fc"), "a b/c");
        assert_eq!(form_decode("plain"), "plain");
        assert_eq!(form_decode("%zz%41"), "%zzA");
    }

    fn callback_addr(listener: &LoopbackCallback) -> std::net::SocketAddr {
        url::Url::parse(listener.redirect_uri())
            .unwrap()
            .socket_addrs(|| None)
            .unwrap()[0]
    }

    #[tokio::test]
    async fn captures_a_valid_callback_and_answers_the_browser() {
        let listener =
            LoopbackCallback::bind("state-1", Utc::now() + chrono::Duration::minutes(5)).unwrap();
        let mut stream = tokio::net::TcpStream::connect(callback_addr(&listener))
            .await
            .unwrap();
        stream
            .write_all(b"GET /callback?code=code-1&state=state-1 HTTP/1.1\r\nHost: x\r\n\r\n")
            .await
            .unwrap();
        let mut page = Vec::new();
        stream.read_to_end(&mut page).await.unwrap();
        assert!(String::from_utf8_lossy(&page).contains("Signed in"));
        for _ in 0..50 {
            if listener.peek().is_some() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("callback outcome was not captured");
    }

    #[tokio::test]
    async fn ignores_a_state_mismatch_and_keeps_waiting() {
        let listener =
            LoopbackCallback::bind("state-1", Utc::now() + chrono::Duration::minutes(5)).unwrap();
        let mut stream = tokio::net::TcpStream::connect(callback_addr(&listener))
            .await
            .unwrap();
        stream
            .write_all(b"GET /callback?code=code-1&state=wrong HTTP/1.1\r\nHost: x\r\n\r\n")
            .await
            .unwrap();
        let mut page = Vec::new();
        stream.read_to_end(&mut page).await.unwrap();
        assert!(String::from_utf8_lossy(&page).contains("400 Bad Request"));
        assert!(listener.peek().is_none());
    }
}
