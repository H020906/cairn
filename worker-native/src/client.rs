//! Just enough HTTP to be a volunteer, over `std::net`.
//!
//! # Why this is hand-written when `api.rs` refused to hand-write its server
//!
//! The coordinator parses requests from the open internet, which is why it uses `tiny_http`:
//! untrusted parsing at a boundary is exactly where a dependency earns its place, and it is the
//! same reasoning that fuzzes `validate.rs`.
//!
//! This is the mirror image. It speaks to **one** server, over a connection it opened itself,
//! and it needs three verbs. Reaching for a client stack would mean an async runtime and a TLS
//! implementation — a hundred crates — to save the fifty lines below, against a project rule
//! that a dependency has to do something the standard library cannot.
//!
//! So the trade is stated rather than hidden: this is a client for a Cairn coordinator, not an
//! HTTP client. It sends `Connection: close` and reads to end of stream, which sidesteps chunked
//! transfer encoding and keep-alive entirely — the two parts of HTTP/1.1 that a short
//! implementation gets wrong.
//!
//! # What it does not do
//!
//! **No TLS.** `https://` is refused rather than silently downgraded. A volunteer on a hostile
//! network can be fed work units and have its answers rewritten; the honest fix is a reverse
//! proxy or a real client crate, and pretending otherwise by accepting the scheme and ignoring
//! it would be worse than refusing.
//!
//! No redirects, no cookies, no compression, no proxies, no IPv6 literal in the URL.

use std::io::{Read as _, Write as _};
use std::net::TcpStream;
use std::time::Duration;

/// Give up on a coordinator that goes quiet.
///
/// Generous, because one of these requests is a party's answer to a challenge and the other end
/// may be mid-adjudication. Not unbounded, because a volunteer that hangs forever on a dead
/// coordinator is a volunteer that has silently left the network.
const TIMEOUT: Duration = Duration::from_secs(120);

/// What came back.
pub struct Response {
    /// The HTTP status code.
    pub status: u16,
    /// The body, however long it turned out to be.
    pub body: Vec<u8>,
}

impl Response {
    /// The body as text, for the JSON the coordinator writes.
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// `GET url`.
///
/// # Errors
///
/// Anything that stops a request completing, as a message fit to print.
pub fn get(url: &str) -> Result<Response, String> {
    request("GET", url, None)
}

/// `POST url` with a body.
///
/// # Errors
///
/// Anything that stops a request completing, as a message fit to print.
pub fn post(url: &str, body: &[u8]) -> Result<Response, String> {
    request("POST", url, Some(body))
}

fn request(method: &str, url: &str, body: Option<&[u8]>) -> Result<Response, String> {
    if url.starts_with("https://") {
        return Err(format!(
            "{url}: this client speaks no TLS, and downgrading silently would be worse than \
             refusing — put a reverse proxy in front of the coordinator, or reach for a real \
             HTTP client"
        ));
    }
    let rest = url.strip_prefix("http://").unwrap_or(url);
    let (authority, path) = match rest.find('/') {
        Some(at) => rest.split_at(at),
        None => (rest, "/"),
    };
    let authority = if authority.contains(':') {
        authority.to_owned()
    } else {
        format!("{authority}:80")
    };

    let stream =
        TcpStream::connect(&authority).map_err(|e| format!("could not reach {authority}: {e}"))?;
    stream
        .set_read_timeout(Some(TIMEOUT))
        .and_then(|()| stream.set_write_timeout(Some(TIMEOUT)))
        .map_err(|e| format!("could not set a timeout: {e}"))?;
    let mut stream = stream;

    let mut head =
        format!("{method} {path} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n");
    if let Some(body) = body {
        head.push_str(&format!(
            "Content-Type: text/plain\r\nContent-Length: {}\r\n",
            body.len()
        ));
    }
    head.push_str("\r\n");

    stream
        .write_all(head.as_bytes())
        .and_then(|()| stream.write_all(body.unwrap_or_default()))
        .and_then(|()| stream.flush())
        .map_err(|e| format!("could not send: {e}"))?;

    let mut raw = Vec::new();
    stream
        .read_to_end(&mut raw)
        .map_err(|e| format!("could not read the reply: {e}"))?;

    // `Connection: close` means the body is everything after the blank line. No `Content-Length`
    // to trust, no chunked framing to reassemble — the two places a short HTTP implementation
    // goes wrong are simply not on this path.
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| "the reply had no header terminator".to_owned())?;
    let head = String::from_utf8_lossy(raw.get(..split).unwrap_or_default()).into_owned();
    let body = raw.get(split + 4..).unwrap_or_default().to_vec();

    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| format!("could not read a status from {head:?}"))?;

    Ok(Response { status, body })
}

/// Pull one field out of the coordinator's JSON.
///
/// **This is not a JSON parser and must not be mistaken for one.** It reads a handful of fields
/// out of documents this repository also writes, in `coordinator/src/api.rs`, whose shapes are
/// fixed and flat. Anything unexpected is `None`, which the caller turns into an error rather
/// than a guess.
///
/// A real parser would be right for a real client. It would also be sixty crates, and this
/// project's rule is that a dependency has to do something the standard library cannot — so the
/// narrowness is the point, and stating it here is what keeps somebody from reaching for this
/// the next time they have some JSON.
#[must_use]
pub fn field(json: &str, key: &str) -> Option<String> {
    let at = json.find(&format!("\"{key}\":"))? + key.len() + 3;
    let rest = json.get(at..)?;
    Some(match rest.strip_prefix('"') {
        // Quoted: everything to the closing quote. The API escapes what it writes, and none of
        // the fields this reads can contain one.
        Some(text) => text.split('"').next()?.to_owned(),
        // Bare: a number, up to the next separator.
        None => rest.split([',', '}']).next()?.trim().to_owned(),
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::field;

    #[test]
    fn reads_the_fields_the_coordinator_writes() {
        let lease = r#"{"unit":3,"workload":"abc123","input":"7a7b"}"#;
        assert_eq!(field(lease, "unit").unwrap(), "3");
        assert_eq!(field(lease, "workload").unwrap(), "abc123");
        assert_eq!(field(lease, "input").unwrap(), "7a7b");

        let challenge =
            r#"{"dispute":0,"unit":1,"token":9,"ask":"root","step":512,"workload":"z","input":""}"#;
        assert_eq!(field(challenge, "ask").unwrap(), "root");
        assert_eq!(field(challenge, "step").unwrap(), "512");
        assert_eq!(field(challenge, "token").unwrap(), "9");
        assert_eq!(
            field(challenge, "input").unwrap(),
            "",
            "an empty input is a value, not a missing field"
        );
    }

    #[test]
    fn an_absent_field_is_none_rather_than_a_guess() {
        assert!(field(r#"{"unit":1}"#, "workload").is_none());
        assert!(field("", "unit").is_none());
        assert!(field("not json at all", "unit").is_none());
    }
}
