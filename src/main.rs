use axum::{
    Router,
    body::Body,
    extract::{DefaultBodyLimit, FromRequest, Request, State},
    http::StatusCode,
    response::Response,
    routing::{get, post},
};
use futures_util::{StreamExt, stream};
use serde_json::{Value, json};
use std::{collections::HashSet, net::IpAddr, sync::Arc, time::Duration};
use tokio::sync::{RwLock, Semaphore, mpsc};

mod live;

const MAX_BODY: usize = 1024 * 1024;
const MAX_WIRE: usize = 1024 * 1024;
const MAX_FRAME: usize = 64 * 1024;
const MAX_EVENTS: usize = 8192;
const MAX_TEXT: usize = 512 * 1024;
const DEADLINE: Duration = Duration::from_secs(10);

struct App {
    key: String,
    live: Option<live::Credentials>,
    upstream: reqwest::Url,
    client: reqwest::Client,
    slots: Arc<Semaphore>,
    catalog: RwLock<Option<(std::time::Instant, Vec<Value>)>>,
    quota: RwLock<Option<(std::time::Instant, Value)>>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let (live, upstream) = match args.next().as_deref() {
        Some("--live") => (Some(live::bootstrap()?), reqwest::Url::parse(live::CHAT)?),
        Some("--offline-upstream") => (
            None,
            offline_url(&args.next().ok_or("missing synthetic upstream")?)?,
        ),
        _ => return Err("expected --live or --offline-upstream ADDRESS".into()),
    };
    if args.next().is_some() {
        return Err("unexpected argument".into());
    }
    let key = std::env::var("DEVIN_HELPER_KEY")?;
    if key.len() < 32 || !key.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return Err(
            "DEVIN_HELPER_KEY must contain at least 32 ASCII alphanumeric characters".into(),
        );
    }
    let app = Arc::new(App {
        key,
        live,
        upstream,
        client: reqwest::Client::builder()
            .no_proxy()
            .retry(reqwest::retry::never())
            .redirect(reqwest::redirect::Policy::none())
            .build()?,
        slots: Arc::new(Semaphore::new(5)),
        catalog: RwLock::new(None),
        quota: RwLock::new(None),
    });
    let router = router(app);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    println!("{}", listener.local_addr()?);
    axum::serve(listener, router).await?;
    Ok(())
}

fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/v1/chat/completions", post(chat))
        .route("/v1/models", get(models))
        .route("/v1/usage", get(usage))
        .layer(DefaultBodyLimit::max(MAX_BODY))
        .with_state(app)
}

fn offline_url(raw: &str) -> Result<reqwest::Url, &'static str> {
    let url = reqwest::Url::parse(raw).map_err(|_| "invalid synthetic upstream")?;
    let ip = url.host_str().and_then(|s| s.parse::<IpAddr>().ok());
    if url.scheme() != "http"
        || !ip.is_some_and(|ip| ip.is_loopback())
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(
            "synthetic upstream must be a literal loopback HTTP address without credentials/query/fragment",
        );
    }
    Ok(url)
}

fn reject(status: StatusCode, message: &'static str) -> Response {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Body::from(
            json!({"error":{"message":message,"type":"offline_bridge_error"}}).to_string(),
        ))
        .unwrap()
}

async fn catalog(app: &App) -> Result<Vec<Value>, &'static str> {
    let fresh = |entry: &Option<(std::time::Instant, Vec<Value>)>| {
        entry
            .as_ref()
            .filter(|(at, _)| at.elapsed() < Duration::from_secs(900))
            .map(|(_, models)| models.clone())
    };
    if let Some(models) = fresh(&*app.catalog.read().await) {
        return Ok(models);
    }
    let mut cache = app.catalog.write().await;
    if let Some(models) = fresh(&cache) {
        return Ok(models);
    }
    let credentials = app
        .live
        .as_ref()
        .ok_or("model discovery requires live credentials")?;
    // Never reuse an expired catalog after a failed refresh.
    *cache = None;
    let models = live::fetch_models(&app.client, credentials).await?;
    *cache = Some((std::time::Instant::now(), models.clone()));
    Ok(models)
}

async fn models(State(app): State<Arc<App>>, request: Request) -> Response {
    if request
        .headers()
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        != Some(&format!("Bearer {}", app.key))
    {
        return reject(StatusCode::UNAUTHORIZED, "authentication required");
    }
    let _permit = match app.slots.clone().try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => return reject(StatusCode::TOO_MANY_REQUESTS, "concurrency limit reached"),
    };
    match catalog(&app).await {
        Ok(data) => Response::builder()
            .header("content-type", "application/json")
            .header("cache-control", "no-store")
            .body(Body::from(json!({"object":"list","data":data}).to_string()))
            .unwrap(),
        Err(message) => reject(StatusCode::BAD_GATEWAY, message),
    }
}

async fn usage(State(app): State<Arc<App>>, request: Request) -> Response {
    if request
        .headers()
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        != Some(&format!("Bearer {}", app.key))
    {
        return reject(StatusCode::UNAUTHORIZED, "authentication required");
    }
    let Some(credentials) = app.live.as_ref() else {
        return reject(StatusCode::BAD_GATEWAY, "quota requires live credentials");
    };
    let mut cache = app.quota.write().await;
    let data = match cache
        .as_ref()
        .filter(|(at, _)| at.elapsed() < Duration::from_secs(60))
    {
        Some((_, data)) => data.clone(),
        None => {
            *cache = None;
            match live::fetch_quota(&app.client, credentials).await {
                Ok(data) => {
                    *cache = Some((std::time::Instant::now(), data.clone()));
                    data
                }
                Err(message) => return reject(StatusCode::BAD_GATEWAY, message),
            }
        }
    };
    Response::builder()
        .header("content-type", "application/json")
        .header("cache-control", "no-store")
        .body(Body::from(data.to_string()))
        .unwrap()
}

async fn chat(State(app): State<Arc<App>>, request: Request) -> Response {
    if request
        .headers()
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        != Some(&format!("Bearer {}", app.key))
    {
        return reject(StatusCode::UNAUTHORIZED, "authentication required");
    }
    let deadline = tokio::time::Instant::now()
        + if app.live.is_some() {
            live::DEADLINE
        } else {
            DEADLINE
        };
    let permit = match app.slots.clone().try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => {
            return reject(
                StatusCode::TOO_MANY_REQUESTS,
                "offline concurrency limit reached",
            );
        }
    };
    let request = match tokio::time::timeout(
        DEADLINE,
        axum::Json::<Value>::from_request(request, &app),
    )
    .await
    {
        Ok(Ok(axum::Json(request))) => request,
        Ok(Err(rejection)) => {
            let status = rejection.status();
            let mut response = reject(
                status,
                if status == StatusCode::PAYLOAD_TOO_LARGE {
                    "Devin request exceeds 1 MiB; reduce attached context before retrying"
                } else {
                    "invalid JSON request body"
                },
            );
            if status == StatusCode::PAYLOAD_TOO_LARGE {
                response
                    .headers_mut()
                    .insert("x-devin-rejected-before-generation", "1".parse().unwrap());
            }
            return response;
        }
        Err(_) => {
            return reject(
                StatusCode::REQUEST_TIMEOUT,
                "request body deadline exceeded",
            );
        }
    };
    let names = match if app.live.is_some() {
        live::validate(&request)
    } else {
        validate_request(&request)
    } {
        Ok(names) => names,
        Err(message) => return reject(StatusCode::BAD_REQUEST, message),
    };
    let mut request = request;
    if app.live.is_some() {
        let catalog = match catalog(&app).await {
            Ok(catalog) => catalog,
            Err(message) => return reject(StatusCode::BAD_GATEWAY, message),
        };
        if let Err(message) = live::select_model(&mut request, &catalog) {
            return reject(StatusCode::BAD_REQUEST, message);
        }
    }
    let (tx, rx) = mpsc::channel::<String>(8);
    tokio::spawn(async move {
        let _permit = permit;
        let work = bridge(&app, request, names, &tx);
        tokio::select! {
            _ = tx.closed() => {},
            result = tokio::time::timeout_at(deadline, work) => {
                let error = match result {
                    Ok(Ok(())) => None,
                    Ok(Err(message)) => Some(message),
                    Err(_) => Some("upstream deadline exceeded"),
                };
                if let Some(message) = error {
                    let _ = tx.try_send(sse(json!({"error":{"message":message,"type":"offline_bridge_error"}})));
                }
            }
        }
    });
    let body = stream::unfold(rx, |mut rx| async {
        rx.recv()
            .await
            .map(|s| (Ok::<_, std::convert::Infallible>(s), rx))
    });
    Response::builder()
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-store")
        .body(Body::from_stream(body))
        .unwrap()
}

fn validate_request(request: &Value) -> Result<HashSet<String>, &'static str> {
    let object = request.as_object().ok_or("request must be an object")?;
    if object.keys().any(|k| {
        ![
            "model",
            "messages",
            "stream",
            "tools",
            "temperature",
            "max_tokens",
            "stream_options",
            "parallel_tool_calls",
        ]
        .contains(&k.as_str())
    }) {
        return Err("unsupported offline request field");
    }
    if request["model"] != "devin-offline" || request["stream"] != true {
        return Err("only streaming devin-offline requests are supported");
    }
    let messages = request["messages"].as_array().ok_or("messages required")?;
    if messages.len() != 1
        || messages[0]["role"] != "user"
        || !messages[0]["content"].is_string()
        || messages[0].as_object().is_none_or(|m| m.len() != 2)
    {
        return Err(
            "offline prototype accepts only one fresh text user turn; replay is not implemented",
        );
    }
    if request
        .get("parallel_tool_calls")
        .is_some_and(|v| !v.is_boolean())
    {
        return Err("parallel_tool_calls must be boolean");
    }
    let mut names = HashSet::new();
    if let Some(tools) = request.get("tools") {
        for tool in tools.as_array().ok_or("tools must be an array")? {
            let name = tool["function"]["name"]
                .as_str()
                .ok_or("tool name required")?;
            if tool["type"] != "function"
                || name.is_empty()
                || !names.insert(name.to_owned())
                || !tool["function"]["parameters"].is_object()
            {
                return Err("invalid or duplicate tool declaration");
            }
        }
    }
    Ok(names)
}

fn sse(value: Value) -> String {
    format!("data: {value}\n\n")
}
fn chunk(delta: Value, finish: Value) -> String {
    sse(
        json!({"id":"chatcmpl-offline","object":"chat.completion.chunk","created":0,"model":"devin-offline",
        "choices":[{"index":0,"delta":delta,"finish_reason":finish}]}),
    )
}

async fn bridge(
    app: &App,
    request: Value,
    names: HashSet<String>,
    tx: &mpsc::Sender<String>,
) -> Result<(), &'static str> {
    let response = if let Some(credentials) = &app.live {
        live::send(&app.client, credentials, &request).await?
    } else {
        app.client
            .post(app.upstream.clone())
            .header("content-type", "application/json")
            .header("accept-encoding", "identity")
            .body(request.to_string())
            .send()
            .await
            .map_err(|_| "synthetic upstream connection failed")?
    };
    relay(
        response,
        names,
        tx,
        app.live.is_some(),
        request["stream_options"]["include_usage"] == true,
        request["model"].as_str().unwrap(),
    )
    .await
}

async fn relay(
    response: reqwest::Response,
    names: HashSet<String>,
    tx: &mpsc::Sender<String>,
    live: bool,
    include_usage: bool,
    model: &str,
) -> Result<(), &'static str> {
    let chunk = |delta, finish| {
        if live {
            live::chunk(model, delta, finish)
        } else {
            chunk(delta, finish)
        }
    };
    if response.status() != reqwest::StatusCode::OK
        || response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            != Some("application/connect+proto")
        || response
            .headers()
            .get("content-encoding")
            .is_some_and(|v| v != "identity")
    {
        return Err("unexpected synthetic upstream response");
    }
    let mut decoder = Decoder::new(names);
    decoder.live = live;
    let mut bytes = response.bytes_stream();
    let idle = if live { live::IDLE } else { DEADLINE };
    while let Some(part) = tokio::time::timeout(idle, bytes.next())
        .await
        .map_err(|_| "upstream idle timeout exceeded")?
    {
        let part = part.map_err(|_| "synthetic upstream read failed")?;
        let mut texts = Vec::new();
        let result = decoder.push_inner(&part, &mut texts);
        for text in texts {
            tx.send(chunk(text.delta(), Value::Null))
                .await
                .map_err(|_| "client cancelled")?;
        }
        result?;
    }
    let usage = decoder.usage.clone();
    let tools = decoder.complete()?;
    for (i, t) in tools.iter().enumerate() {
        let call = json!({"index":i,"id":t.id,"type":"function","function":{"name":t.name,"arguments":t.args}});
        tx.send(chunk(json!({"tool_calls":[call]}), Value::Null))
            .await
            .map_err(|_| "client cancelled")?;
    }
    let finish = json!(if tools.is_empty() {
        "stop"
    } else {
        "tool_calls"
    });
    tx.send(if live && include_usage && !usage.is_null() {
        sse(
            json!({"id":"chatcmpl-devin","object":"chat.completion.chunk","model":model,"created":0,
            "choices":[{"index":0,"delta":{},"finish_reason":finish}],"usage":usage}),
        )
    } else {
        chunk(json!({}), finish)
    })
    .await
    .map_err(|_| "client cancelled")?;
    tx.send("data: [DONE]\n\n".to_owned())
        .await
        .map_err(|_| "client cancelled")?;
    Ok(())
}

#[derive(Debug)]
struct Tool {
    id: String,
    name: String,
    args: String,
}
#[derive(Debug, PartialEq)]
enum Output {
    Text(String),
    Reasoning(String),
}
impl Output {
    fn delta(self) -> Value {
        match self {
            Self::Text(text) => json!({"content": text}),
            Self::Reasoning(text) => json!({"reasoning_content": text}),
        }
    }
}
struct Decoder {
    pending: Vec<u8>,
    wire: usize,
    frames: usize,
    output: usize,
    eos: bool,
    finish: Option<u64>,
    tools: Vec<Tool>,
    names: HashSet<String>,
    live: bool,
    usage: Value,
    usage_counts: Value,
}
impl Decoder {
    fn new(names: HashSet<String>) -> Self {
        Self {
            pending: Vec::new(),
            wire: 0,
            frames: 0,
            output: 0,
            eos: false,
            finish: None,
            tools: Vec::new(),
            names,
            live: false,
            usage: Value::Null,
            usage_counts: json!({}),
        }
    }
    #[cfg(test)]
    fn push(&mut self, bytes: &[u8]) -> Result<Vec<String>, &'static str> {
        let mut texts = Vec::new();
        self.push_inner(bytes, &mut texts)?;
        Ok(texts
            .into_iter()
            .map(|output| match output {
                Output::Text(text) | Output::Reasoning(text) => text,
            })
            .collect())
    }
    fn push_inner(&mut self, bytes: &[u8], texts: &mut Vec<Output>) -> Result<(), &'static str> {
        self.wire = self.wire.checked_add(bytes.len()).ok_or("wire limit")?;
        if self.wire > MAX_WIRE {
            return Err("wire limit");
        }
        for byte in bytes {
            if self.eos {
                return Err("data after trailer");
            }
            self.pending.push(*byte);
            if self.pending.len() >= 5 {
                let n = u32::from_be_bytes(self.pending[1..5].try_into().unwrap()) as usize;
                if n > MAX_FRAME {
                    return Err("frame limit");
                }
                if !matches!(self.pending[0], 0 | 2) {
                    return Err("only identity Connect frames supported");
                }
                if self.pending.len() == n + 5 {
                    self.frames += 1;
                    if self.frames > MAX_EVENTS {
                        return Err("frame count limit");
                    }
                    let frame = std::mem::take(&mut self.pending);
                    if frame[0] == 2 {
                        let trailer: Value = serde_json::from_slice(&frame[5..])
                            .map_err(|_| "invalid trailer JSON")?;
                        if if self.live {
                            !live::valid_trailer(&trailer)
                        } else {
                            trailer != json!({})
                        } {
                            return Err("nonempty or error trailer");
                        }
                        self.eos = true;
                    } else {
                        self.frame(&frame[5..], texts)?;
                    }
                }
            }
        }
        Ok(())
    }
    fn frame(&mut self, bytes: &[u8], texts: &mut Vec<Output>) -> Result<(), &'static str> {
        let finished_before_frame = self.finish.is_some();
        for (number, field) in fields(bytes)? {
            match (number, field) {
                (3, Field::Bytes(b)) | (9, Field::Bytes(b)) if number == 3 || self.live => {
                    if if self.live {
                        finished_before_frame
                    } else {
                        self.finish.is_some()
                    } {
                        return Err("text after finish");
                    }
                    let text = utf8(b)?;
                    self.output += text.len();
                    if self.output > MAX_TEXT {
                        return Err("output limit");
                    }
                    texts.push(if number == 9 {
                        Output::Reasoning(text)
                    } else {
                        Output::Text(text)
                    });
                }
                (5, Field::Varint(n)) => {
                    if self.finish.replace(n).is_some() {
                        return Err("duplicate finish");
                    }
                    match n {
                        0 | 2 | 4 | 6 | 8 | 10 | 12 => {}
                        1 => return Err("upstream finish: incomplete"),
                        3 => return Err("upstream finish: max tokens"),
                        5 => return Err("upstream finish: max newlines"),
                        7 => return Err("upstream finish: nonfinite probability"),
                        9 => return Err("upstream finish: partial"),
                        11 => return Err("upstream finish: content filter"),
                        13 => return Err("upstream finish: error"),
                        _ => return Err("unsuccessful or unknown finish"),
                    }
                }
                (6, Field::Bytes(b)) => {
                    if if self.live {
                        finished_before_frame
                    } else {
                        self.finish.is_some()
                    } {
                        return Err("tool after finish");
                    }
                    let mut id = None;
                    let mut name = None;
                    let mut args = None;
                    for (number, field) in fields(b)? {
                        let Field::Bytes(b) = field else {
                            return Err("invalid tool wire type");
                        };
                        let target = match number {
                            1 => &mut id,
                            2 => &mut name,
                            3 => &mut args,
                            _ => return Err("unsupported tool field"),
                        };
                        if target.replace(utf8(b)?).is_some() {
                            return Err("duplicate tool field");
                        }
                    }
                    if let (Some(id), Some(name)) = (&id, &name) {
                        if id.is_empty()
                            || !self.names.contains(name)
                            || self.tools.iter().any(|t| t.id == *id)
                            || self.tools.len() >= 32
                        {
                            return Err("invalid, unknown or duplicate tool");
                        }
                        self.tools.push(Tool {
                            id: id.clone(),
                            name: name.clone(),
                            args: args.unwrap_or_default(),
                        });
                    } else if id.is_none() && name.is_none() {
                        self.tools
                            .last_mut()
                            .ok_or("orphan tool arguments")?
                            .args
                            .push_str(&args.ok_or("missing tool arguments")?);
                    } else {
                        return Err("incomplete tool identity");
                    }
                }
                (7 | 28, Field::Bytes(b)) if self.live => {
                    let counts = live::read_usage(b, number == 28)?;
                    if counts.is_empty() {
                        continue;
                    }
                    for (key, value) in counts {
                        self.usage_counts[key] = json!(value);
                    }
                    let count = |key| self.usage_counts[key].as_u64().unwrap_or(0);
                    let prompt = count("input_tokens")
                        .checked_add(count("cache_write_tokens"))
                        .and_then(|n| n.checked_add(count("cache_read_tokens")))
                        .ok_or("usage count overflow")?;
                    let completion = count("output_tokens");
                    let total = prompt
                        .checked_add(completion)
                        .ok_or("usage count overflow")?;
                    self.usage = json!({"prompt_tokens":prompt,"completion_tokens":completion,"total_tokens":total});
                    for (source, detail) in [
                        ("cache_read_tokens", "cached_tokens"),
                        ("cache_write_tokens", "cache_write_tokens"),
                    ] {
                        if let Some(value) = self.usage_counts.get(source) {
                            self.usage["prompt_tokens_details"][detail] = value.clone();
                        }
                    }
                }
                (number, field) if self.live => live::response_field(number, field)?,
                _ => return Err("unsupported offline response field or wire type"),
            }
        }
        Ok(())
    }
    fn complete(self) -> Result<Vec<Tool>, &'static str> {
        if !self.eos || !self.pending.is_empty() || self.finish.is_none() {
            return Err("incomplete response");
        }
        if if self.tools.is_empty() {
            self.finish == Some(10)
        } else {
            !self.live && self.finish != Some(10)
        } {
            return Err("finish/tool mismatch");
        }
        for tool in &self.tools {
            let args: Value = serde_json::from_str(&tool.args).map_err(|_| "invalid tool JSON")?;
            if !args.is_object() {
                return Err("tool arguments must be an object");
            }
        }
        Ok(self.tools)
    }
}
fn utf8(bytes: &[u8]) -> Result<String, &'static str> {
    std::str::from_utf8(bytes)
        .map(str::to_owned)
        .map_err(|_| "invalid UTF-8")
}
enum Field<'a> {
    Varint(u64),
    Bytes(&'a [u8]),
    Fixed(&'a [u8]),
}
fn varint(bytes: &[u8], pos: &mut usize) -> Result<u64, &'static str> {
    let mut n = 0;
    for shift in (0..70).step_by(7) {
        let byte = *bytes.get(*pos).ok_or("truncated varint")?;
        *pos += 1;
        if shift == 63 && byte > 1 {
            return Err("varint overflow");
        }
        n |= ((byte & 127) as u64) << shift;
        if byte < 128 {
            return Ok(n);
        }
    }
    Err("varint overflow")
}
fn fields(bytes: &[u8]) -> Result<Vec<(u64, Field<'_>)>, &'static str> {
    fields_limit(bytes, 1024)
}
fn fields_limit(bytes: &[u8], limit: usize) -> Result<Vec<(u64, Field<'_>)>, &'static str> {
    let mut pos = 0;
    let mut out = Vec::new();
    while pos < bytes.len() {
        if out.len() >= limit {
            return Err("field count limit");
        }
        let tag = varint(bytes, &mut pos)?;
        if tag >> 3 == 0 || tag >> 3 > 0x1fff_ffff {
            return Err("invalid field number");
        }
        let field = match tag & 7 {
            0 => Field::Varint(varint(bytes, &mut pos)?),
            2 => {
                let len =
                    usize::try_from(varint(bytes, &mut pos)?).map_err(|_| "field size overflow")?;
                let end = pos.checked_add(len).ok_or("field size overflow")?;
                let value = bytes.get(pos..end).ok_or("truncated field")?;
                pos = end;
                Field::Bytes(value)
            }
            1 | 5 => {
                let width = if tag & 7 == 1 { 8 } else { 4 };
                let end = pos.checked_add(width).ok_or("field size overflow")?;
                let value = bytes.get(pos..end).ok_or("truncated fixed field")?;
                pos = end;
                Field::Fixed(value)
            }
            _ => return Err("unsupported wire type"),
        };
        out.push((tag >> 3, field));
    }
    Ok(out)
}

#[cfg(test)]
mod tests;
