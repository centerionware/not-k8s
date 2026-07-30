use super::*;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[tokio::test]
async fn tcp_check_succeeds_against_a_listening_port() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let _ = listener.accept().await;
    });
    assert!(check_tcp("127.0.0.1", port, Duration::from_secs(1)).await);
}

#[tokio::test]
async fn tcp_check_fails_against_a_closed_port() {
    // Bind then drop immediately to get a port nothing is listening on.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    assert!(!check_tcp("127.0.0.1", port, Duration::from_millis(300)).await);
}

#[tokio::test]
async fn tcp_check_treats_port_zero_as_unresolved_and_fails_fast() {
    let start = std::time::Instant::now();
    assert!(!check_tcp("127.0.0.1", 0, Duration::from_secs(5)).await);
    assert!(start.elapsed() < Duration::from_secs(1), "unresolved port must not wait for the timeout");
}

#[tokio::test]
async fn http_check_succeeds_on_2xx_response() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        if let Ok((mut sock, _)) = listener.accept().await {
            let mut buf = [0u8; 512];
            let _ = sock.read(&mut buf).await;
            let _ = sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n").await;
        }
    });
    assert!(check_http("127.0.0.1", port, "/healthz", Duration::from_secs(1)).await);
}

#[tokio::test]
async fn http_check_fails_on_5xx_response() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        if let Ok((mut sock, _)) = listener.accept().await {
            let mut buf = [0u8; 512];
            let _ = sock.read(&mut buf).await;
            let _ = sock.write_all(b"HTTP/1.1 500 Internal Server Error\r\n\r\n").await;
        }
    });
    assert!(!check_http("127.0.0.1", port, "/healthz", Duration::from_secs(1)).await);
}

#[tokio::test]
async fn http_check_fails_when_nothing_is_listening() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    assert!(!check_http("127.0.0.1", port, "/", Duration::from_millis(300)).await);
}
