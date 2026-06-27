//! Thin H3-object codec — the L7 relay spine (design doc §5.3).
//!
//! Encodes/decodes the *head* of an HTTP request or response — method, target,
//! version, headers — to a compact, length-delimited byte buffer. It is **not**
//! HTTP/1.1 (no parser, no chunked grammar) and **not** H3+QPACK (no dynamic
//! table); it is the minimal wire object both relay legs share.
//!
//! **HTTP-version-agnostic.** HTTP/1.1, HTTP/2, and HTTP/3 requests all
//! decompose to the same `(method, target, headers, body)` — only their wire
//! encoding and multiplexing differ, and both terminate at the edge. So one
//! object format carries all three (§2).
//!
//! ## Wire layout
//!
//! ```text
//! request head:  [u8 fmt=1][u8 ver][len+method][len+uri][headers]
//! response head: [u8 fmt=1][u8 ver][u16 status][headers]
//! headers:       [u32 count] then count × ([len+name][len+value])
//! len:           u32 LE byte-count prefix
//! ```
//!
//! The head is one self-contained buffer. On an Aster stream it is written as a
//! single length-prefixed frame; the body then streams as subsequent frames
//! until the sender finishes the stream (that body pumping is the dispatch
//! layer's job — step A4/A5 — not this module's).

use anyhow::{bail, Context, Result};
use http::header::{HeaderName, HeaderValue};
use http::{request, response, HeaderMap, Method, Request, Response, StatusCode, Uri, Version};

/// Wire-format version. Bump when the head layout changes (e.g. when RFC 9218
/// priority is added in Stage C); the decoder rejects unknown versions.
const FORMAT_VERSION: u8 = 1;

// ── http::Version ↔ wire code ───────────────────────────────────────────────

fn ver_code(v: Version) -> u8 {
    match v {
        Version::HTTP_09 => 0,
        Version::HTTP_10 => 1,
        Version::HTTP_11 => 2,
        Version::HTTP_2 => 3,
        Version::HTTP_3 => 4,
        // `http::Version` is non-exhaustive; default unknowns to HTTP/1.1.
        _ => 2,
    }
}

fn code_ver(c: u8) -> Version {
    match c {
        0 => Version::HTTP_09,
        1 => Version::HTTP_10,
        2 => Version::HTTP_11,
        3 => Version::HTTP_2,
        4 => Version::HTTP_3,
        _ => Version::HTTP_11,
    }
}

// ── encode ──────────────────────────────────────────────────────────────────

fn put_bytes(out: &mut Vec<u8>, b: &[u8]) -> Result<()> {
    let len: u32 = b.len().try_into().context("field exceeds u32 length")?;
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(b);
    Ok(())
}

fn put_headers(out: &mut Vec<u8>, headers: &HeaderMap) -> Result<()> {
    // `iter()` yields one entry per value, repeating the name for multi-valued
    // headers (e.g. Set-Cookie); `append` on decode rebuilds them in order.
    let count: u32 = headers
        .iter()
        .count()
        .try_into()
        .context("too many headers")?;
    out.extend_from_slice(&count.to_le_bytes());
    for (name, value) in headers.iter() {
        put_bytes(out, name.as_str().as_bytes())?;
        put_bytes(out, value.as_bytes())?;
    }
    Ok(())
}

/// Encode an HTTP request head to a self-contained byte buffer.
pub fn encode_request_head(parts: &request::Parts) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(64);
    out.push(FORMAT_VERSION);
    out.push(ver_code(parts.version));
    put_bytes(&mut out, parts.method.as_str().as_bytes())?;
    put_bytes(&mut out, parts.uri.to_string().as_bytes())?;
    put_headers(&mut out, &parts.headers)?;
    Ok(out)
}

/// Encode an HTTP response head to a self-contained byte buffer.
pub fn encode_response_head(parts: &response::Parts) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(64);
    out.push(FORMAT_VERSION);
    out.push(ver_code(parts.version));
    out.extend_from_slice(&parts.status.as_u16().to_le_bytes());
    put_headers(&mut out, &parts.headers)?;
    Ok(out)
}

// ── decode ──────────────────────────────────────────────────────────────────

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.pos.checked_add(n).context("length overflow")?;
        let slice = self
            .buf
            .get(self.pos..end)
            .context("unexpected end of head")?;
        self.pos = end;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16> {
        let s = self.take(2)?;
        Ok(u16::from_le_bytes([s[0], s[1]]))
    }

    fn u32(&mut self) -> Result<u32> {
        let s = self.take(4)?;
        Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
    }

    /// Read a `[u32 len][bytes]` length-delimited field.
    fn lpfx(&mut self) -> Result<&'a [u8]> {
        let n = self.u32()? as usize;
        self.take(n)
    }

    /// Ensure the whole buffer was consumed — trailing bytes are a framing bug.
    fn finish(self) -> Result<()> {
        if self.pos != self.buf.len() {
            bail!("{} trailing byte(s) in head", self.buf.len() - self.pos);
        }
        Ok(())
    }
}

fn read_fmt(r: &mut Reader) -> Result<()> {
    let fmt = r.u8()?;
    if fmt != FORMAT_VERSION {
        bail!("unsupported H3-object format version {fmt} (expected {FORMAT_VERSION})");
    }
    Ok(())
}

fn read_headers(r: &mut Reader) -> Result<HeaderMap> {
    let count = r.u32()? as usize;
    let mut headers = HeaderMap::with_capacity(count);
    for _ in 0..count {
        let name = HeaderName::from_bytes(r.lpfx()?).context("invalid header name")?;
        let value = HeaderValue::from_bytes(r.lpfx()?).context("invalid header value")?;
        headers.append(name, value);
    }
    Ok(headers)
}

/// Decode a request head produced by [`encode_request_head`].
pub fn decode_request_head(buf: &[u8]) -> Result<request::Parts> {
    let mut r = Reader::new(buf);
    read_fmt(&mut r)?;
    let version = code_ver(r.u8()?);
    let method = Method::from_bytes(r.lpfx()?).context("invalid method")?;
    let uri: Uri = std::str::from_utf8(r.lpfx()?)
        .context("uri not utf-8")?
        .parse()
        .context("invalid uri")?;
    let headers = read_headers(&mut r)?;
    r.finish()?;

    let mut req = Request::new(());
    *req.method_mut() = method;
    *req.uri_mut() = uri;
    *req.version_mut() = version;
    *req.headers_mut() = headers;
    Ok(req.into_parts().0)
}

/// Decode a response head produced by [`encode_response_head`].
pub fn decode_response_head(buf: &[u8]) -> Result<response::Parts> {
    let mut r = Reader::new(buf);
    read_fmt(&mut r)?;
    let version = code_ver(r.u8()?);
    let status = StatusCode::from_u16(r.u16()?).context("invalid status code")?;
    let headers = read_headers(&mut r)?;
    r.finish()?;

    let mut resp = Response::new(());
    *resp.status_mut() = status;
    *resp.version_mut() = version;
    *resp.headers_mut() = headers;
    Ok(resp.into_parts().0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req_parts() -> request::Parts {
        let mut req = Request::builder()
            .method("POST")
            .uri("/api/items?page=2&q=hello%20world")
            .version(Version::HTTP_3)
            .header("host", "backend.local")
            .header("accept", "application/json")
            // Latin-1 / obs-text bytes are legal in header values.
            .header("x-note", HeaderValue::from_bytes(b"caf\xC3\xA9").unwrap())
            .body(())
            .unwrap();
        // Two values for one name — must round-trip in order.
        req.headers_mut()
            .append("set-cookie", HeaderValue::from_static("a=1"));
        req.headers_mut()
            .append("set-cookie", HeaderValue::from_static("b=2"));
        req.into_parts().0
    }

    #[test]
    fn request_head_round_trips() {
        let parts = req_parts();
        let buf = encode_request_head(&parts).unwrap();
        let got = decode_request_head(&buf).unwrap();

        assert_eq!(got.method, parts.method);
        assert_eq!(got.uri, parts.uri);
        assert_eq!(got.version, parts.version);
        assert_eq!(got.headers, parts.headers);

        let cookies: Vec<_> = got.headers.get_all("set-cookie").iter().collect();
        assert_eq!(cookies, vec!["a=1", "b=2"]);
    }

    #[test]
    fn response_head_round_trips() {
        let parts = Response::builder()
            .status(404)
            .version(Version::HTTP_2)
            .header("content-type", "text/plain")
            .header("x-trace", "abc123")
            .body(())
            .unwrap()
            .into_parts()
            .0;

        let buf = encode_response_head(&parts).unwrap();
        let got = decode_response_head(&buf).unwrap();

        assert_eq!(got.status, StatusCode::NOT_FOUND);
        assert_eq!(got.version, Version::HTTP_2);
        assert_eq!(got.headers, parts.headers);
    }

    #[test]
    fn empty_headers_round_trip() {
        let parts = Request::builder()
            .method("GET")
            .uri("/")
            .body(())
            .unwrap()
            .into_parts()
            .0;
        let buf = encode_request_head(&parts).unwrap();
        let got = decode_request_head(&buf).unwrap();
        assert_eq!(got.method, Method::GET);
        assert!(got.headers.is_empty());
    }

    #[test]
    fn truncated_buffer_errors() {
        let parts = req_parts();
        let buf = encode_request_head(&parts).unwrap();
        assert!(decode_request_head(&buf[..buf.len() / 2]).is_err());
    }

    #[test]
    fn trailing_bytes_error() {
        let parts = req_parts();
        let mut buf = encode_request_head(&parts).unwrap();
        buf.push(0); // one byte past the head
        assert!(decode_request_head(&buf).is_err());
    }

    #[test]
    fn unknown_format_version_errors() {
        let parts = req_parts();
        let mut buf = encode_request_head(&parts).unwrap();
        buf[0] = 99;
        assert!(decode_request_head(&buf).is_err());
    }
}
