use super::*;
use axum::http::HeaderMap;

fn json_request(headers: HeaderMap, value: Value) -> Request {
    let mut request = Request::new(Body::from(value.to_string()));
    *request.headers_mut() = headers;
    request
        .headers_mut()
        .insert("content-type", "application/json".parse().unwrap());
    DefaultBodyLimit::max(MAX_BODY).apply(&mut request);
    request
}

fn frame(flag: u8, bytes: &[u8]) -> Vec<u8> {
    let mut out = vec![flag];
    out.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    out.extend_from_slice(bytes);
    out
}
fn text(s: &str) -> Vec<u8> {
    let mut out = vec![26, s.len() as u8];
    out.extend_from_slice(s.as_bytes());
    frame(0, &out)
}
fn tool(args: &str) -> Vec<u8> {
    named_tool(b'a', b'f', args)
}
fn named_tool(id: u8, name: u8, args: &str) -> Vec<u8> {
    let mut inner = vec![10, 1, id, 18, 1, name, 26, args.len() as u8];
    inner.extend_from_slice(args.as_bytes());
    let mut out = vec![50, inner.len() as u8];
    out.extend(inner);
    frame(0, &out)
}
fn decode(bytes: &[u8]) -> Result<Vec<Tool>, &'static str> {
    let mut decoder = Decoder::new(HashSet::from(["f".to_owned()]));
    for byte in bytes {
        decoder.push(&[*byte])?;
    }
    decoder.complete()
}
fn response(args: &str) -> Vec<u8> {
    [tool(args), frame(0, &[40, 10]), frame(2, b"{}")].concat()
}
#[test]
fn fragmented_text_and_tools() {
    let bytes = [text("hello"), frame(0, &[40, 2]), frame(2, b"{}")].concat();
    let mut decoder = Decoder::new(HashSet::new());
    let mut output = String::new();
    for byte in bytes {
        output.extend(decoder.push(&[byte]).unwrap());
    }
    assert_eq!(output, "hello");
    assert!(decoder.complete().unwrap().is_empty());
    let tools = decode(&response(r#"{"x":"é"}"#)).unwrap();
    assert_eq!(tools[0].args, r#"{"x":"é"}"#);
}
#[test]
fn incomplete_or_invalid_terminal_never_commits_tools() {
    let valid = response("{}");
    for end in 0..valid.len() {
        assert!(decode(&valid[..end]).is_err(), "prefix {end}");
    }
    for trailer in [
        b"bad".as_slice(),
        b"null",
        b"[]",
        b"{\"error\":null}",
        b"{\"error\":{}}",
    ] {
        assert!(decode(&[tool("{}"), frame(0, &[40, 10]), frame(2, trailer)].concat()).is_err());
    }
    for reason in [1, 3, 7, 11, 13, 99] {
        assert!(decode(&[tool("{}"), frame(0, &[40, reason]), frame(2, b"{}")].concat()).is_err());
    }
    assert!(decode(&[tool("{}"), frame(2, b"{}")].concat()).is_err());
    assert!(decode(&[valid, vec![0]].concat()).is_err());
}
#[test]
fn unsuccessful_finishes_preserve_reason_and_withhold_tools() {
    for (reason, expected) in [
        (1, "upstream finish: incomplete"),
        (3, "upstream finish: max tokens"),
        (5, "upstream finish: max newlines"),
        (7, "upstream finish: nonfinite probability"),
        (9, "upstream finish: partial"),
        (11, "upstream finish: content filter"),
        (13, "upstream finish: error"),
        (14, "unsuccessful or unknown finish"),
    ] {
        let bytes = [tool("{}"), frame(0, &[40, reason]), frame(2, b"{}")].concat();
        let mut decoder = Decoder::new(HashSet::from(["f".into()]));
        decoder.live = true;
        assert_eq!(decoder.push(&bytes).unwrap_err(), expected);
    }
}

#[test]
fn finish_tool_consistency_live_and_offline() {
    for live in [false, true] {
        for with_tool in [false, true] {
            for reason in 0..=14 {
                let bytes = [
                    if with_tool { tool("{}") } else { vec![] },
                    frame(0, &[40, reason]),
                    frame(2, b"{}"),
                ]
                .concat();
                let mut decoder = Decoder::new(HashSet::from(["f".into()]));
                decoder.live = live;
                let result = decoder.push(&bytes).and_then(|_| decoder.complete());
                let normal_stop = matches!(reason, 0 | 2 | 4 | 6 | 8 | 12);
                let expected = if with_tool {
                    reason == 10 || (live && normal_stop)
                } else {
                    normal_stop
                };
                assert_eq!(
                    result.is_ok(),
                    expected,
                    "live={live} tool={with_tool} reason={reason}"
                );
                if expected {
                    assert_eq!(result.unwrap().len(), usize::from(with_tool));
                }
            }
        }
    }
}
#[test]
fn invalid_tool_arguments_and_identity_refused() {
    for args in ["{broken", "[]", "null", "1", "", "{} trailing"] {
        assert!(decode(&response(args)).is_err());
    }
    assert!(
        decode(&[tool("{}"), tool("{}"), frame(0, &[40, 10]), frame(2, b"{}")].concat()).is_err()
    );
    let mut decoder = Decoder::new(HashSet::new());
    assert!(decoder.push(&tool("{}")).is_err());
    assert!(decode(&[tool("{}"), frame(0, &[40, 2]), frame(2, b"{}")].concat()).is_err());
}
#[test]
fn wire_and_structure_bounds() {
    assert!(decode(&frame(1, b"gzip")).is_err());
    assert!(decode(&frame(4, b"unknown")).is_err());
    let mut decoder = Decoder::new(HashSet::new());
    let mut header = vec![0];
    header.extend_from_slice(&((MAX_FRAME + 1) as u32).to_be_bytes());
    assert!(decoder.push(&header).is_err());
    assert!(fields(&[0]).is_err());
    assert!(fields(&[26, 10]).is_err());
    assert!(fields(&[40, 255, 255, 255, 255, 255, 255, 255, 255, 255, 2]).is_err());
    assert!(decode(&frame(0, &[26, 1, 255])).is_err());
    let mut decoder = Decoder::new(HashSet::new());
    assert!(decoder.push(&vec![0; MAX_STREAM + 1]).is_err());
}
#[test]
fn offline_boundary_and_replay_refusal() {
    assert!(offline_url("http://127.0.0.1:1234/fixture").is_ok());
    for raw in [
        "https://127.0.0.1",
        "http://example.com",
        "http://localhost",
        "http://127.0.0.1?secret=x",
        "http://x@127.0.0.1",
    ] {
        assert!(offline_url(raw).is_err());
    }
    let mut req = json!({"model":"devin-offline","stream":true,"messages":[{"role":"user","content":"hello"}]});
    assert!(validate_request(&req).is_ok());
    req["messages"]
        .as_array_mut()
        .unwrap()
        .push(json!({"role":"assistant","content":"old"}));
    assert!(validate_request(&req).is_err());
}

#[tokio::test]
async fn authentication_and_admission_precede_upstream() {
    let app = Arc::new(App {
        key: "x".repeat(32),
        live: None,
        upstream: offline_url("http://127.0.0.1:1").unwrap(),
        client: reqwest::Client::builder().no_proxy().build().unwrap(),
        slots: Arc::new(Semaphore::new(0)),
        catalog: RwLock::new(None),
        quota: RwLock::new(None),
    });
    let request = json!({"model":"devin-offline","stream":true,"messages":[{"role":"user","content":"synthetic"}]});
    let response = chat(
        State(app.clone()),
        json_request(HeaderMap::new(), request.clone()),
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let mut headers = HeaderMap::new();
    headers.insert(
        "authorization",
        format!("Bearer {}", app.key).parse().unwrap(),
    );
    let response = chat(State(app), json_request(headers, request)).await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
}

struct HttpHelper {
    app: Arc<App>,
    address: std::net::SocketAddr,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for HttpHelper {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl HttpHelper {
    async fn start() -> Self {
        let app = Arc::new(App {
            key: "x".repeat(32),
            live: None,
            upstream: offline_url("http://127.0.0.1:1").unwrap(),
            client: reqwest::Client::builder().no_proxy().build().unwrap(),
            slots: Arc::new(Semaphore::new(1)),
            catalog: RwLock::new(None),
            quota: RwLock::new(None),
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let router = router(app.clone());
        let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        Self { app, address, task }
    }

    async fn request(
        &self,
        authorization: &str,
        length: usize,
        body: &str,
    ) -> tokio::net::TcpStream {
        use tokio::io::AsyncWriteExt;
        let mut socket = tokio::net::TcpStream::connect(self.address).await.unwrap();
        socket
            .write_all(format!("POST /v1/chat/completions HTTP/1.1\r\nHost: localhost\r\n{authorization}Content-Type: application/json\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n{body}").as_bytes())
            .await
            .unwrap();
        socket
    }

    fn authorization(&self) -> String {
        format!("Authorization: Bearer {}\r\n", self.app.key)
    }
}

async fn http_status(socket: &mut tokio::net::TcpStream, deadline: Duration) -> u16 {
    use tokio::io::AsyncReadExt;
    tokio::time::timeout(deadline, async {
        let mut line = Vec::new();
        loop {
            let byte = socket.read_u8().await.unwrap();
            line.push(byte);
            if byte == b'\n' {
                break;
            }
            assert!(line.len() < 128);
        }
        String::from_utf8(line)
            .unwrap()
            .split_whitespace()
            .nth(1)
            .unwrap()
            .parse()
            .unwrap()
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn http_incomplete_unauthenticated_bodies_are_rejected_without_admission() {
    let helper = HttpHelper::start().await;
    let permit = helper.app.slots.acquire().await.unwrap();
    for authorization in ["", "Authorization: Bearer invalid\r\n"] {
        let mut socket = helper.request(authorization, 100, "{").await;
        assert_eq!(http_status(&mut socket, Duration::from_secs(2)).await, 401);
        assert_eq!(helper.app.slots.available_permits(), 0);
    }
    drop(permit);
    permits_recovered(&helper.app).await;
}

#[tokio::test]
async fn http_stalled_body_holds_admission_until_timeout() {
    let helper = HttpHelper::start().await;
    let authorization = helper.authorization();
    let mut stalled = helper.request(&authorization, 100, "{").await;
    tokio::time::timeout(Duration::from_secs(2), async {
        while helper.app.slots.available_permits() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let mut excess = helper.request(&authorization, 100, "{").await;
    assert_eq!(http_status(&mut excess, Duration::from_secs(2)).await, 429);
    assert_eq!(helper.app.slots.available_permits(), 0);
    assert_eq!(
        http_status(&mut stalled, DEADLINE + Duration::from_secs(2)).await,
        408
    );
    permits_recovered(&helper.app).await;
    let mut malformed = helper.request(&authorization, 1, "{").await;
    assert_eq!(
        http_status(&mut malformed, Duration::from_secs(2)).await,
        400
    );
    permits_recovered(&helper.app).await;
}

#[tokio::test]
async fn http_body_size_and_disconnect_release_admission() {
    let helper = HttpHelper::start().await;
    let authorization = helper.authorization();
    let body = " ".repeat(MAX_BODY + 1);
    let mut oversized = helper.request(&authorization, body.len(), &body).await;
    assert_eq!(
        http_status(&mut oversized, Duration::from_secs(2)).await,
        413
    );
    permits_recovered(&helper.app).await;
    let stalled = helper.request(&authorization, 100, "{").await;
    tokio::time::timeout(Duration::from_secs(2), async {
        while helper.app.slots.available_permits() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    drop(stalled);
    permits_recovered(&helper.app).await;
}

#[test]
fn valid_partial_text_survives_a_later_frame_error() {
    let mut decoder = Decoder::new(HashSet::new());
    let mut texts = Vec::new();
    assert!(
        decoder
            .push_inner(&[text("visible"), frame(2, b"bad")].concat(), &mut texts)
            .is_err()
    );
    assert_eq!(texts, [Output::Text("visible".into())]);
}

#[tokio::test]
async fn downstream_drop_cancels_synthetic_upstream() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let upstream = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buf = [0; 4096];
        let _ = socket.read(&mut buf).await.unwrap();
        socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/connect+proto\r\nTransfer-Encoding: chunked\r\n\r\n").await.unwrap();
        let _ = started_tx.send(());
        loop {
            match socket.read(&mut buf).await {
                Ok(0) => break,
                Ok(_) => {}
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted
                    ) =>
                {
                    break;
                }
                Err(e) => panic!("unexpected socket error: {e}"),
            }
        }
    });
    let app = Arc::new(App {
        key: "x".repeat(32),
        live: None,
        upstream: offline_url(&format!("http://{address}")).unwrap(),
        client: reqwest::Client::builder().no_proxy().build().unwrap(),
        slots: Arc::new(Semaphore::new(5)),
        catalog: RwLock::new(None),
        quota: RwLock::new(None),
    });
    let mut headers = HeaderMap::new();
    headers.insert(
        "authorization",
        format!("Bearer {}", app.key).parse().unwrap(),
    );
    let response = chat(State(app.clone()), json_request(headers, json!({"model":"devin-offline","stream":true,"messages":[{"role":"user","content":"synthetic"}]}))).await;
    tokio::time::timeout(Duration::from_secs(2), started_rx)
        .await
        .unwrap()
        .unwrap();
    drop(response);
    tokio::time::timeout(Duration::from_secs(2), upstream)
        .await
        .unwrap()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while app.slots.available_permits() != 5 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

struct SyntheticUpstream {
    app: Arc<App>,
    task: tokio::task::JoinHandle<()>,
    sent: tokio::sync::oneshot::Receiver<()>,
}
impl Drop for SyntheticUpstream {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn synthetic_upstream(payload: Vec<u8>, complete: bool) -> SyntheticUpstream {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (sent_tx, sent) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let mut buf = [0; 4096];
        loop {
            let n = socket.read(&mut buf).await.unwrap();
            assert_ne!(n, 0);
            request.extend_from_slice(&buf[..n]);
            if let Some(end) = request.windows(4).position(|b| b == b"\r\n\r\n") {
                let headers = std::str::from_utf8(&request[..end]).unwrap();
                let length: usize = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse().unwrap())
                    })
                    .unwrap();
                if request.len() >= end + 4 + length {
                    break;
                }
            }
            assert!(request.len() < MAX_BODY);
        }
        socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/connect+proto\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n").await.unwrap();
        if !payload.is_empty() {
            socket
                .write_all(format!("{:x}\r\n", payload.len()).as_bytes())
                .await
                .unwrap();
            socket.write_all(&payload).await.unwrap();
            socket.write_all(b"\r\n").await.unwrap();
        }
        if complete {
            socket.write_all(b"0\r\n\r\n").await.unwrap();
        }
        let _ = sent_tx.send(());
        if !complete {
            loop {
                match socket.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(_) => {}
                    Err(e)
                        if matches!(
                            e.kind(),
                            std::io::ErrorKind::ConnectionReset
                                | std::io::ErrorKind::ConnectionAborted
                        ) =>
                    {
                        break;
                    }
                    Err(e) => panic!("unexpected socket error: {e}"),
                }
            }
        }
    });
    SyntheticUpstream {
        app: Arc::new(App {
            key: "x".repeat(32),
            live: None,
            upstream: offline_url(&format!("http://{address}")).unwrap(),
            client: reqwest::Client::builder().no_proxy().build().unwrap(),
            slots: Arc::new(Semaphore::new(1)),
            catalog: RwLock::new(None),
            quota: RwLock::new(None),
        }),
        task,
        sent,
    }
}
fn synthetic_request() -> Value {
    json!({"model":"devin-offline","stream":true,"messages":[{"role":"user","content":"synthetic"}],
        "tools":[{"type":"function","function":{"name":"f","parameters":{}}},
                 {"type":"function","function":{"name":"g","parameters":{}}}]})
}
async fn synthetic_chat(app: &Arc<App>) -> Response {
    let mut headers = HeaderMap::new();
    headers.insert(
        "authorization",
        format!("Bearer {}", app.key).parse().unwrap(),
    );
    let response = chat(
        State(app.clone()),
        json_request(headers, synthetic_request()),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    response
}
async fn permits_recovered(app: &App) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while app.slots.available_permits() != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let permit = app.slots.try_acquire().unwrap();
    assert_eq!(app.slots.available_permits(), 0);
    drop(permit);
}

#[tokio::test]
async fn multiple_unique_tools_commit_only_after_complete_valid_response() {
    let payload = [
        named_tool(b'a', b'f', r#"{"x":1}"#),
        named_tool(b'b', b'g', r#"{"y":2}"#),
        frame(0, &[40, 10]),
        frame(2, b"{}"),
    ]
    .concat();
    let fixture = synthetic_upstream(payload, true).await;
    let response = synthetic_chat(&fixture.app).await;
    let body = tokio::time::timeout(
        Duration::from_secs(2),
        axum::body::to_bytes(response.into_body(), MAX_WIRE),
    )
    .await
    .unwrap()
    .unwrap();
    let body = std::str::from_utf8(&body).unwrap();
    let events: Vec<_> = body.split("\n\n").filter(|s| !s.is_empty()).collect();
    assert_eq!(events.len(), 4);
    for (index, id, name, args) in [(0, "a", "f", r#"{"x":1}"#), (1, "b", "g", r#"{"y":2}"#)] {
        let event: Value =
            serde_json::from_str(events[index].strip_prefix("data: ").unwrap()).unwrap();
        assert_eq!(
            event["choices"][0]["delta"]["tool_calls"],
            json!([{"index":index,"id":id,"type":"function","function":{"name":name,"arguments":args}}])
        );
        assert!(event["choices"][0]["finish_reason"].is_null());
    }
    let finish: Value = serde_json::from_str(events[2].strip_prefix("data: ").unwrap()).unwrap();
    assert_eq!(finish["choices"][0]["finish_reason"], "tool_calls");
    assert_eq!(events[3], "data: [DONE]");
    permits_recovered(&fixture.app).await;
}

#[tokio::test]
async fn late_malformed_data_blocks_every_buffered_tool() {
    for tail in [
        frame(0, &[0]),
        frame(2, b"bad"),
        vec![0],
        named_tool(b'c', b'g', "[]"),
    ] {
        let payload = [
            text("visible"),
            named_tool(b'a', b'f', "{}"),
            named_tool(b'b', b'g', "{}"),
            tail,
            frame(0, &[40, 10]),
            frame(2, b"{}"),
        ]
        .concat();
        let fixture = synthetic_upstream(payload, true).await;
        let response = synthetic_chat(&fixture.app).await;
        let body = tokio::time::timeout(
            Duration::from_secs(2),
            axum::body::to_bytes(response.into_body(), MAX_WIRE),
        )
        .await
        .unwrap()
        .unwrap();
        let body = std::str::from_utf8(&body).unwrap();
        assert!(body.contains("visible"));
        assert!(body.contains("offline_bridge_error"));
        assert!(!body.contains("tool_calls"));
        assert!(!body.contains("[DONE]"));
        permits_recovered(&fixture.app).await;
    }
}

#[tokio::test]
async fn full_downstream_channel_cancellation_releases_chat_permit() {
    let payload = (0..32).flat_map(|_| text("blocked")).collect();
    let mut fixture = synthetic_upstream(payload, false).await;
    let response = synthetic_chat(&fixture.app).await;
    tokio::time::timeout(Duration::from_secs(2), &mut fixture.sent)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(fixture.app.slots.available_permits(), 0);
    let mut body = response.into_body().into_data_stream();
    let first = tokio::time::timeout(Duration::from_secs(2), body.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(std::str::from_utf8(&first).unwrap().contains("blocked"));
    let second = tokio::time::timeout(Duration::from_secs(2), body.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(std::str::from_utf8(&second).unwrap().contains("blocked"));
    assert_eq!(fixture.app.slots.available_permits(), 0);
    drop(body);
    tokio::time::timeout(Duration::from_secs(2), &mut fixture.task)
        .await
        .unwrap()
        .unwrap();
    permits_recovered(&fixture.app).await;
}

#[tokio::test]
async fn chat_deadline_closes_stalled_upstream_and_releases_permit_with_full_downstream() {
    let payload = (0..32).flat_map(|_| text("blocked")).collect();
    let mut fixture = synthetic_upstream(payload, false).await;
    let started = tokio::time::Instant::now();
    let response = synthetic_chat(&fixture.app).await;
    tokio::time::timeout(Duration::from_secs(2), &mut fixture.sent)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(fixture.app.slots.available_permits(), 0);
    tokio::time::timeout(DEADLINE + Duration::from_secs(2), &mut fixture.task)
        .await
        .unwrap()
        .unwrap();
    assert!(started.elapsed() >= DEADLINE);
    permits_recovered(&fixture.app).await;
    let body = tokio::time::timeout(
        Duration::from_secs(2),
        axum::body::to_bytes(response.into_body(), MAX_WIRE),
    )
    .await
    .unwrap()
    .unwrap();
    let body = std::str::from_utf8(&body).unwrap();
    let events: Vec<_> = body.split("\n\n").filter(|s| !s.is_empty()).collect();
    assert_eq!(events.len(), 8);
    for event in events {
        let event: Value = serde_json::from_str(event.strip_prefix("data: ").unwrap()).unwrap();
        assert_eq!(event["choices"][0]["delta"]["content"], "blocked");
        assert!(event["choices"][0]["finish_reason"].is_null());
    }
    assert!(!body.contains("offline_bridge_error"));
    assert!(!body.contains("[DONE]"));
}

#[tokio::test]
async fn full_channel_bridge_send_is_cancelled_by_short_timeout() {
    let mut fixture = synthetic_upstream(text("blocked"), false).await;
    let (tx, mut rx) = mpsc::channel(1);
    tx.send("occupied".to_owned()).await.unwrap();
    let mut work = Box::pin(bridge(
        &fixture.app,
        synthetic_request(),
        HashSet::new(),
        &tx,
    ));
    tokio::time::timeout(Duration::from_secs(2), async {
        tokio::select! {
            result = &mut work => panic!("bridge completed while channel was full: {result:?}"),
            result = &mut fixture.sent => result.unwrap(),
        }
    })
    .await
    .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(20), work)
            .await
            .is_err()
    );
    tokio::time::timeout(Duration::from_secs(2), &mut fixture.task)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(rx.recv().await.unwrap(), "occupied");
    assert!(matches!(
        rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
}

#[tokio::test]
async fn short_bridge_timeout_cancels_stalled_upstream_without_chat_deadline() {
    let mut fixture = synthetic_upstream(Vec::new(), false).await;
    let (tx, mut rx) = mpsc::channel(1);
    let request = synthetic_request();
    let mut work = Box::pin(bridge(&fixture.app, request, HashSet::new(), &tx));
    tokio::time::timeout(Duration::from_secs(2), async {
        tokio::select! {
            result = &mut work => panic!("bridge completed before timeout: {result:?}"),
            result = &mut fixture.sent => result.unwrap(),
        }
    })
    .await
    .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(20), work)
            .await
            .is_err()
    );
    tokio::time::timeout(Duration::from_secs(2), &mut fixture.task)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    assert_eq!(fixture.app.slots.available_permits(), 1);
}

fn metric_usage(name: &str, value: f32) -> Vec<u8> {
    let mut value_wire = vec![21];
    value_wire.extend(value.to_le_bytes());
    let mut metric = vec![34, value_wire.len() as u8];
    metric.extend(value_wire);
    metric.extend([42, name.len() as u8]);
    metric.extend(name.as_bytes());
    let mut metrics = vec![18, metric.len() as u8];
    metrics.extend(metric);
    let mut response = vec![226, 1, metrics.len() as u8];
    response.extend(metrics);
    frame(0, &response)
}

#[test]
fn cache_usage_merges_cumulative_partial_native_and_metrics() {
    for metrics_first in [false, true] {
        let mut d = Decoder::new(HashSet::new());
        d.live = true;
        let native = frame(0, &[58, 8, 16, 3, 24, 2, 32, 5, 40, 7]);
        let metrics = [
            metric_usage("input_tokens", 3.0),
            metric_usage("output_tokens", 2.0),
            metric_usage("cache_write_tokens", 5.0),
            metric_usage("cache_read_tokens", 7.0),
        ]
        .concat();
        for snapshot in if metrics_first {
            [&metrics, &native]
        } else {
            [&native, &metrics]
        } {
            for _ in 0..3 {
                for byte in snapshot {
                    d.push(&[*byte]).unwrap();
                }
                assert_eq!(
                    d.usage,
                    json!({"prompt_tokens":15,"completion_tokens":2,"total_tokens":17,
                    "prompt_tokens_details":{"cached_tokens":7,"cache_write_tokens":5}})
                );
            }
        }
        d.push(&metric_usage("output_tokens", 4.0)).unwrap();
        assert_eq!(d.usage["total_tokens"], 19);
        d.push(&frame(0, &[58, 2, 40, 9])).unwrap();
        assert_eq!(d.usage["prompt_tokens"], 17);
        assert_eq!(d.usage["total_tokens"], 21);
        d.push(&metric_usage("cache_read_tokens", 0.0)).unwrap();
        assert_eq!(d.usage["prompt_tokens_details"]["cached_tokens"], 0);
        assert_eq!(d.usage["prompt_tokens_details"]["cache_write_tokens"], 5);
        assert_eq!(d.usage["total_tokens"], 12);
    }
}

#[test]
fn cache_usage_does_not_invent_missing_counters() {
    let mut d = Decoder::new(HashSet::new());
    d.live = true;
    for empty in [
        frame(0, &[58, 0]),
        metric_usage("unrelated", 3.0),
        metric_usage("unrelated", 1.5),
        metric_usage("unrelated", f32::MAX),
    ] {
        d.push(&empty).unwrap();
        assert!(d.usage.is_null());
    }
    d.push(&frame(0, &[58, 4, 16, 3, 24, 2])).unwrap();
    assert_eq!(
        d.usage,
        json!({"prompt_tokens":3,"completion_tokens":2,"total_tokens":5})
    );
    d.push(&metric_usage("cache_write_tokens", 5.0)).unwrap();
    assert_eq!(
        d.usage["prompt_tokens_details"],
        json!({"cache_write_tokens":5})
    );
    assert_eq!(d.usage["prompt_tokens"], 8);
    assert_eq!(d.usage["total_tokens"], 10);
    d.push(&metric_usage("cache_creation_tokens", 6.0)).unwrap();
    d.push(&metric_usage("cached_tokens", 7.0)).unwrap();
    assert_eq!(
        d.usage,
        json!({"prompt_tokens":16,"completion_tokens":2,"total_tokens":18,
            "prompt_tokens_details":{"cache_write_tokens":6,"cached_tokens":7}})
    );
}

#[test]
fn cache_usage_rejects_invalid_numbers_and_overflow() {
    for value in [
        f32::NAN,
        f32::INFINITY,
        -1.0,
        -0.0,
        1.5,
        18446744073709551616.0,
    ] {
        for metric in [
            "input_tokens",
            "output_tokens",
            "cache_read_tokens",
            "cache_write_tokens",
            "cache_creation_tokens",
            "cached_tokens",
            "unrelated",
        ] {
            let mut d = Decoder::new(HashSet::new());
            d.live = true;
            assert_eq!(
                d.push(&metric_usage(metric, value)).is_err(),
                metric != "unrelated" || !value.is_finite() || value.is_sign_negative(),
                "{metric}: {value}"
            );
        }
    }
    let mut d = Decoder::new(HashSet::new());
    d.live = true;
    d.push(&metric_usage(
        "cache_read_tokens",
        f32::from_bits((2.0_f32.powi(64)).to_bits() - 1),
    ))
    .unwrap();
    assert_eq!(d.usage["prompt_tokens"], 18446742974197923840_u64);
    for extra in [24, 32, 40] {
        let mut d = Decoder::new(HashSet::new());
        d.live = true;
        let mut stats = vec![16];
        stats.extend([255; 9]);
        stats.push(1);
        stats.extend([extra, 1]);
        let mut response = vec![58, stats.len() as u8];
        response.extend(stats);
        assert_eq!(d.push(&frame(0, &response)), Err("usage count overflow"));
    }
}

#[tokio::test]
async fn live_relay_synthetic_http_eof_and_usage() {
    for (valid, reason, include_usage) in [
        (true, 0, true),
        (true, 2, true),
        (true, 4, true),
        (true, 6, true),
        (true, 8, true),
        (true, 10, true),
        (true, 12, true),
        (false, 2, true),
        (false, 13, true),
        (true, 2, false),
    ] {
        let payload = [
            text("BETA_OK"),
            frame(0, &[58, 4, 32, 5, 40, 7]),
            metric_usage("cache_read_tokens", 7.0),
            frame(
                0,
                &[
                    40, reason, 50, 10, 10, 1, b'i', 18, 1, b'f', 26, 2, b'{', b'}', 58, 4, 16, 3,
                    24, 2,
                ],
            ),
            frame(2, br#"{"metadata":{"x":["y"]}}"#),
            if valid { vec![] } else { vec![0] },
        ]
        .concat();
        let fixture = synthetic_upstream(payload, true).await;
        let response = fixture
            .app
            .client
            .post(fixture.app.upstream.clone())
            .body("{}")
            .send()
            .await
            .unwrap();
        let (tx, mut rx) = mpsc::channel(16);
        let result = relay(
            response,
            HashSet::from(["f".into()]),
            &tx,
            true,
            include_usage,
            if reason % 4 == 0 {
                "swe-2-flash-medium"
            } else {
                "second-model"
            },
        )
        .await;
        drop(tx);
        let mut output = String::new();
        while let Some(part) = rx.recv().await {
            output.push_str(&part);
        }
        assert!(output.contains("BETA_OK"));
        let model = if reason % 4 == 0 {
            "swe-2-flash-medium"
        } else {
            "second-model"
        };
        for event in output
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
        {
            if event == "[DONE]" {
                continue;
            }
            let event: Value = serde_json::from_str(event).unwrap();
            assert_eq!(event["model"], model);
        }
        assert_eq!(result.is_ok(), valid);
        assert_eq!(output.contains("[DONE]"), valid);
        assert_eq!(output.contains("tool_calls"), valid);
        assert_eq!(output.contains("\"usage\":"), valid && include_usage);
        if valid && include_usage {
            assert_eq!(output.matches("\"usage\":").count(), 1);
            let events: Vec<_> = output
                .lines()
                .filter_map(|line| line.strip_prefix("data: "))
                .collect();
            assert_eq!(events.last(), Some(&"[DONE]"));
            let event: Value = serde_json::from_str(events[events.len() - 2]).unwrap();
            assert_eq!(
                event["choices"],
                json!([{"index":0,"delta":{},"finish_reason":"tool_calls"}])
            );
            assert_eq!(
                event["usage"],
                json!({"prompt_tokens":15,"completion_tokens":2,"total_tokens":17,
                "prompt_tokens_details":{"cached_tokens":7,"cache_write_tokens":5}})
            );
        }
    }
}

#[tokio::test]
async fn unsigned_reasoning_stream_and_late_signatures_gate_tools() {
    for tail in [
        None,
        Some((10, false)),
        Some((20, false)),
        Some((20, true)),
        Some((21, false)),
        Some((11, true)),
    ] {
        let mut reasoning = vec![74, "visible thinking 🦀".len() as u8];
        reasoning.extend("visible thinking 🦀".as_bytes());
        let mut payload = [
            frame(0, &reasoning),
            text("answer"),
            named_tool(b'a', b'f', "{}"),
            frame(0, &[40, 10]),
        ]
        .concat();
        if let Some((number, varint)) = tail {
            let signature = if varint {
                if number == 20 {
                    vec![160, 1, 1]
                } else {
                    vec![88, 1]
                }
            } else {
                let mut field = match number {
                    10 => vec![82],
                    20 => vec![162, 1],
                    _ => vec![170, 1],
                };
                field.extend([6]);
                field.extend(b"signed");
                field
            };
            payload.extend(frame(0, &signature));
        }
        payload.extend(frame(2, b"{}"));
        let fixture = synthetic_upstream(payload, true).await;
        let response = fixture
            .app
            .client
            .post(fixture.app.upstream.clone())
            .body("{}")
            .send()
            .await
            .unwrap();
        let (tx, mut rx) = mpsc::channel(16);
        let result = relay(
            response,
            HashSet::from(["f".into()]),
            &tx,
            true,
            false,
            "second-model",
        )
        .await;
        drop(tx);
        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            events.push(event);
        }
        let reasoning: Value =
            serde_json::from_str(events[0].trim().strip_prefix("data: ").unwrap()).unwrap();
        assert_eq!(
            reasoning["choices"][0]["delta"],
            json!({"reasoning_content":"visible thinking 🦀"})
        );
        let text: Value =
            serde_json::from_str(events[1].trim().strip_prefix("data: ").unwrap()).unwrap();
        assert_eq!(text["choices"][0]["delta"], json!({"content":"answer"}));
        if let Some((number, varint)) = tail {
            assert_eq!(
                result,
                Err(match number {
                    10 => "nonempty signature unsupported",
                    20 if !varint => "nonempty Gemini signature unsupported",
                    21 => "nonempty signature type unsupported",
                    _ => "unsupported reasoning or incorrect wire type",
                })
            );
            assert_eq!(events.len(), 2);
            assert!(!events.concat().contains("tool_calls"));
            assert!(!events.concat().contains("[DONE]"));
        } else {
            result.unwrap();
            assert_eq!(events.len(), 5);
            assert!(events[2].contains("tool_calls"));
            assert_eq!(events[4], "data: [DONE]\n\n");
        }
    }
}

#[test]
fn unsigned_reasoning_is_bounded_and_retained_before_late_error() {
    let payload = [vec![74, 8], b"thinking".to_vec()].concat();
    let mut decoder = Decoder::new(HashSet::new());
    decoder.live = true;
    let mut output = Vec::new();
    assert!(
        decoder
            .push_inner(
                &[frame(0, &payload), frame(0, &[80, 1])].concat(),
                &mut output
            )
            .is_err()
    );
    assert_eq!(output, [Output::Reasoning("thinking".into())]);
    let mut decoder = Decoder::new(HashSet::new());
    decoder.live = true;
    decoder.output = MAX_TEXT - "thinking".len();
    decoder
        .push_inner(&frame(0, &payload), &mut Vec::new())
        .unwrap();
    assert_eq!(decoder.output, MAX_TEXT);
    assert_eq!(decoder.push(&text("x")), Err("output limit"));
    let mut decoder = Decoder::new(HashSet::new());
    decoder.live = true;
    decoder.output = MAX_TEXT;
    assert_eq!(decoder.push(&frame(0, &payload)), Err("output limit"));
    let mut decoder = Decoder::new(HashSet::new());
    decoder.live = true;
    decoder.push(&frame(0, &[40, 0])).unwrap();
    assert_eq!(decoder.push(&frame(0, &payload)), Err("text after finish"));
}

#[tokio::test]
async fn request_capacity_and_json_rejections() {
    let app = Arc::new(App {
        key: "x".repeat(32),
        live: None,
        upstream: offline_url("http://127.0.0.1:1").unwrap(),
        client: reqwest::Client::builder().no_proxy().build().unwrap(),
        slots: Arc::new(Semaphore::new(1)),
        catalog: RwLock::new(None),
        quota: RwLock::new(None),
    });
    let mut headers = HeaderMap::new();
    headers.insert(
        "authorization",
        format!("Bearer {}", app.key).parse().unwrap(),
    );
    let mut request =
        json!({"model":"devin-offline","stream":true,"messages":[{"role":"user","content":""}]});
    let overhead = request.to_string().len();
    request["messages"][0]["content"] = Value::String("x".repeat(MAX_BODY - overhead));
    assert_eq!(request.to_string().len(), MAX_BODY);
    let response = chat(
        State(app.clone()),
        json_request(headers.clone(), request.clone()),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    drop(response);
    permits_recovered(&app).await;
    request["messages"][0]["content"] = Value::String("x".repeat(MAX_BODY - overhead + 1));
    let response = chat(State(app.clone()), json_request(headers.clone(), request)).await;
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(
        response.headers()["x-devin-rejected-before-generation"],
        "1"
    );
    assert_eq!(response.headers()["content-type"], "application/json");
    let bytes = axum::body::to_bytes(response.into_body(), 4096)
        .await
        .unwrap();
    let error: Value = serde_json::from_slice(&bytes).unwrap();
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("1 MiB")
    );
    permits_recovered(&app).await;
    let mut malformed = Request::new(Body::from("{"));
    *malformed.headers_mut() = headers;
    malformed
        .headers_mut()
        .insert("content-type", "application/json".parse().unwrap());
    let response = chat(State(app.clone()), malformed).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(
        !response
            .headers()
            .contains_key("x-devin-rejected-before-generation")
    );
    let bytes = axum::body::to_bytes(response.into_body(), 4096)
        .await
        .unwrap();
    let error: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(error["error"]["message"], "invalid JSON request body");
    permits_recovered(&app).await;
}

#[tokio::test]
async fn usage_requires_authentication_and_live_credentials() {
    let helper = HttpHelper::start().await;
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let url = format!("http://{}/v1/usage", helper.address);
    assert_eq!(
        client.get(&url).send().await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );
    *helper.app.quota.write().await = Some((std::time::Instant::now(), json!({"daily":{}})));
    assert_eq!(
        client
            .get(&url)
            .header("authorization", format!("Bearer {}", helper.app.key))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_GATEWAY
    );
}

#[tokio::test]
async fn authenticated_models_http_and_cache_expiry_fail_closed() {
    let helper = HttpHelper::start().await;
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let url = format!("http://{}/v1/models", helper.address);
    assert_eq!(
        client.get(&url).send().await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );
    let get = || {
        client
            .get(&url)
            .header("authorization", format!("Bearer {}", helper.app.key))
    };
    assert_eq!(
        get().send().await.unwrap().status(),
        StatusCode::BAD_GATEWAY
    );
    let data = vec![
        json!({"id":"one","object":"model","owned_by":"devin","context_window":10000,"max_output_tokens":4096,"capabilities":{"tool_use":true,"images":false}}),
    ];
    *helper.app.catalog.write().await = Some((std::time::Instant::now(), data.clone()));
    let response = get().send().await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["cache-control"], "no-store");
    let body: Value = serde_json::from_slice(&response.bytes().await.unwrap()).unwrap();
    assert_eq!(body, json!({"object":"list","data":data}));
    *helper.app.catalog.write().await =
        Some((std::time::Instant::now() - Duration::from_secs(901), data));
    assert_eq!(
        get().send().await.unwrap().status(),
        StatusCode::BAD_GATEWAY
    );
}
