//! One HTTP/1.1 request as the metrics socket reads it: the request line, the
//! form body of a browser's POST, and the query and form values.

use tokio::io::AsyncReadExt;

/// The most header bytes read before the request is taken as it stands.
const MAX_HEAD: usize = 8 * 1024;
/// The most body bytes read.
const MAX_BODY: usize = 4 * 1024;

/// A parsed request. A malformed or empty one has an empty method and path.
#[derive(Debug, Default)]
pub(super) struct Request {
    pub(super) method: String,
    pub(super) path: String,
    /// The raw `a=b&c=d` after `?` (empty if none).
    pub(super) query: String,
    /// The `application/x-www-form-urlencoded` body of a form post, if any.
    pub(super) form: Option<String>,
}

impl Request {
    /// The value of `key` in the query, else in the form body.
    pub(super) fn param(&self, key: &str) -> Option<String> {
        value_of(&self.query, key).or_else(|| self.form.as_deref().and_then(|f| value_of(f, key)))
    }

    /// Whether the request is a browser form post, answered by a redirect.
    pub(super) fn is_form(&self) -> bool {
        self.form.is_some()
    }
}

/// Read one request off `sock`: the head up to its blank line, then the body
/// its `Content-Length` announces (both bounded).
pub(super) async fn read_request<S: AsyncReadExt + Unpin>(sock: &mut S) -> Request {
    let mut buf = Vec::with_capacity(2048);
    let mut chunk = [0u8; 2048];
    let head_end = loop {
        if let Some(at) = find(&buf, b"\r\n\r\n") {
            break Some(at + 4);
        }
        if buf.len() >= MAX_HEAD {
            break None;
        }
        match sock.read(&mut chunk).await {
            Ok(0) | Err(_) => break None,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    };
    let head_len = head_end.unwrap_or(buf.len());
    let head = String::from_utf8_lossy(&buf[..head_len]).into_owned();
    let mut req = parse_head(&head);
    let length = header(&head, "content-length").and_then(|v| v.parse::<usize>().ok());
    let is_form = header(&head, "content-type")
        .is_some_and(|v| v.starts_with("application/x-www-form-urlencoded"));
    if let (Some(start), Some(length), true) = (head_end, length, is_form) {
        let want = length.min(MAX_BODY);
        let mut body = buf[start..].to_vec();
        while body.len() < want {
            match sock.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => body.extend_from_slice(&chunk[..n]),
            }
        }
        body.truncate(want);
        req.form = Some(String::from_utf8_lossy(&body).into_owned());
    }
    req
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// The request line of `head` as `(METHOD, path, query)`.
fn parse_head(head: &str) -> Request {
    let line = head.lines().next().unwrap_or("");
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("");
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (percent_decoded(p), q.to_string()),
        None => (percent_decoded(target), String::new()),
    };
    Request { method, path, query, form: None }
}

/// `path` with its `%XX` escapes decoded; an invalid escape stays as it is.
fn percent_decoded(path: &str) -> String {
    let bytes = path.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = bytes.get(i + 1..i + 3).and_then(|h| std::str::from_utf8(h).ok());
        match (bytes[i], hex.and_then(|h| u8::from_str_radix(h, 16).ok())) {
            (b'%', Some(b)) => {
                out.push(b);
                i += 3;
            }
            (b, _) => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The value of header `name` (lower case) in `head`.
fn header<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.lines().skip(1).find_map(|line| {
        let (k, v) = line.split_once(':')?;
        k.trim().eq_ignore_ascii_case(name).then(|| v.trim())
    })
}

/// Pull `key`'s value out of a raw `a=b&c=d` string (no percent-decoding:
/// the values read are numbers).
fn value_of(pairs: &str, key: &str) -> Option<String> {
    pairs.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k == key && !v.is_empty()).then(|| v.to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore = "slow lane: loadgen"]
    async fn a_form_post_split_across_reads_yields_its_body() {
        let (mut client, mut server) = tokio::io::duplex(64);
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            let head = "POST /rate HTTP/1.1\r\nHost: x\r\nContent-Type: \
                        application/x-www-form-urlencoded\r\nContent-Length: 6\r\n\r\n";
            client.write_all(head.as_bytes()).await.unwrap();
            client.write_all(b"cps=40").await.unwrap();
        });
        let req = read_request(&mut server).await;
        assert_eq!((req.method.as_str(), req.path.as_str()), ("POST", "/rate"));
        assert!(req.is_form());
        assert_eq!(req.param("cps").as_deref(), Some("40"));
    }

    #[test]
    #[ignore = "slow lane: loadgen"]
    fn the_path_is_percent_decoded() {
        let req = parse_head("GET /callflows/s/c/a%20b@x/0.html%2 HTTP/1.1\r\n");
        assert_eq!(req.path, "/callflows/s/c/a b@x/0.html%2");
    }

    #[tokio::test]
    #[ignore = "slow lane: loadgen"]
    async fn a_query_post_has_no_form() {
        let mut input: &[u8] = b"POST /rate?cps=2.5 HTTP/1.1\r\nHost: x\r\n\r\n";
        let req = read_request(&mut input).await;
        assert!(!req.is_form());
        assert_eq!(req.param("cps").as_deref(), Some("2.5"));
    }
}
