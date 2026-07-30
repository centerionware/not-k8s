use super::*;

#[test]
fn accepts_2xx_and_3xx() {
    assert_eq!(parse_http_status_ok(b"HTTP/1.1 200 OK\r\n"), Some(true));
    assert_eq!(parse_http_status_ok(b"HTTP/1.1 204 No Content\r\n"), Some(true));
    assert_eq!(parse_http_status_ok(b"HTTP/1.0 301 Moved\r\n"), Some(true));
    assert_eq!(parse_http_status_ok(b"HTTP/1.1 399 Whatever\r\n"), Some(true));
}

#[test]
fn rejects_4xx_and_5xx() {
    assert_eq!(parse_http_status_ok(b"HTTP/1.1 404 Not Found\r\n"), Some(false));
    assert_eq!(parse_http_status_ok(b"HTTP/1.1 500 Internal Server Error\r\n"), Some(false));
    assert_eq!(parse_http_status_ok(b"HTTP/1.1 400 Bad Request\r\n"), Some(false));
}

#[test]
fn garbage_input_is_unparseable() {
    assert_eq!(parse_http_status_ok(b""), None);
    assert_eq!(parse_http_status_ok(b"not an http response"), None);
    assert_eq!(parse_http_status_ok(b"HTTP/1.1 not-a-code OK\r\n"), None);
}
