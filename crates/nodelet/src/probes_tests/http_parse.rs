use super::*;

#[test]
fn accepts_2xx_and_3xx() {
    assert_eq!(parse_http_status(b"HTTP/1.1 200 OK\r\n"), Some(200));
    assert_eq!(parse_http_status(b"HTTP/1.1 204 No Content\r\n"), Some(204));
    assert_eq!(parse_http_status(b"HTTP/1.0 301 Moved\r\n"), Some(301));
    assert_eq!(parse_http_status(b"HTTP/1.1 399 Whatever\r\n"), Some(399));
}

#[test]
fn rejects_4xx_and_5xx() {
    assert_eq!(parse_http_status(b"HTTP/1.1 404 Not Found\r\n"), Some(404));
    assert_eq!(parse_http_status(b"HTTP/1.1 500 Internal Server Error\r\n"), Some(500));
    assert_eq!(parse_http_status(b"HTTP/1.1 400 Bad Request\r\n"), Some(400));
}

#[test]
fn garbage_input_is_unparseable() {
    assert_eq!(parse_http_status(b""), None);
    assert_eq!(parse_http_status(b"not an http response"), None);
    assert_eq!(parse_http_status(b"HTTP/1.1 not-a-code OK\r\n"), None);
}
