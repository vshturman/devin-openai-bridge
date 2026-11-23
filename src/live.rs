use super::*;
use base64::Engine;
use std::io::{BufRead, Read};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

pub const CHAT: &str =
    "https://server.codeium.com/exa.api_server_pb.ApiServerService/GetChatMessage";
const AUTH: &str = "https://server.codeium.com/exa.auth_pb.AuthService/GetUserJwt";
pub const MODEL: &str = "swe-2-flash-medium";
pub const DEADLINE: Duration = Duration::from_secs(120);
static REQUEST: AtomicU64 = AtomicU64::new(1);

pub struct Credentials {
    api_key: String,
    version: String,
}

pub fn bootstrap() -> Result<Credentials, &'static str> {
    let mut line = Vec::new();
    std::io::stdin()
        .lock()
        .take(16385)
        .read_until(b'\n', &mut line)
        .map_err(|_| "bootstrap read failed")?;
    if line.len() > 16384 || line.last() != Some(&b'\n') {
        return Err("bootstrap must be one bounded JSON line");
    }
    parse_bootstrap(&line)
}
fn parse_bootstrap(line: &[u8]) -> Result<Credentials, &'static str> {
    let value: Value = serde_json::from_slice(line).map_err(|_| "invalid bootstrap JSON")?;
    keys(&value, &["api_key", "client_version"])?;
    let api_key = string(&value["api_key"])?;
    let version = string(&value["client_version"])?;
    if api_key.is_empty()
        || api_key.len() > 8192
        || api_key.chars().any(char::is_control)
        || version.split('.').count() != 3
        || !version
            .split('.')
            .all(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
    {
        return Err("invalid bootstrap credentials/version");
    }
    Ok(Credentials {
        api_key: api_key.into(),
        version: version.into(),
    })
}
fn keys(value: &Value, allowed: &[&str]) -> Result<(), &'static str> {
    if value
        .as_object()
        .ok_or("expected object")?
        .keys()
        .any(|k| !allowed.contains(&k.as_str()))
    {
        return Err("unsupported field (including signed/reasoning state)");
    }
    Ok(())
}
fn string(v: &Value) -> Result<&str, &'static str> {
    v.as_str().ok_or("expected text")
}
fn nonempty(v: &Value) -> Result<&str, &'static str> {
    let s = string(v)?;
    if s.is_empty() {
        Err("expected nonempty text")
    } else {
        Ok(s)
    }
}
fn array(v: &Value) -> Result<&Vec<Value>, &'static str> {
    v.as_array().ok_or("expected array")
}
fn object_json(v: &Value) -> Result<&str, &'static str> {
    let s = string(v)?;
    if !serde_json::from_str::<Value>(s)
        .map_err(|_| "invalid tool arguments")?
        .is_object()
    {
        return Err("tool arguments must be JSON object");
    }
    Ok(s)
}
pub fn validate(r: &Value) -> Result<HashSet<String>, &'static str> {
    keys(
        r,
        &[
            "model",
            "messages",
            "stream",
            "tools",
            "max_tokens",
            "user",
            "stream_options",
            "parallel_tool_calls",
            "temperature",
        ],
    )?;
    if r["model"] != MODEL || r["stream"] != true {
        return Err("only streaming swe-2-flash-medium supported");
    }
    if let Some(n) = r.get("max_tokens")
        && !n.as_u64().is_some_and(|n| (1..=4096).contains(&n))
    {
        return Err("max_tokens must be 1..4096");
    }
    if let Some(user) = r.get("user") {
        let s = nonempty(user)?;
        if s.len() > 64 || !s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
            return Err("invalid session user");
        }
    }
    if r.get("temperature")
        .is_some_and(|v| v.as_f64() != Some(1.0))
    {
        return Err("only default temperature supported");
    }
    if r.get("parallel_tool_calls")
        .is_some_and(|v| v != &Value::Bool(true))
    {
        return Err("only default parallel_tool_calls supported");
    }
    if let Some(options) = r.get("stream_options") {
        keys(options, &["include_usage"])?;
        if options
            .get("include_usage")
            .is_some_and(|v| !v.is_boolean())
        {
            return Err("invalid stream_options");
        }
    }
    let mut names = HashSet::new();
    if let Some(tools) = r.get("tools") {
        for t in array(tools)? {
            keys(t, &["type", "function"])?;
            keys(&t["function"], &["name", "description", "parameters"])?;
            if t["type"] != "function"
                || !t["function"]["parameters"].is_object()
                || !names.insert(nonempty(&t["function"]["name"])?.to_owned())
            {
                return Err("invalid tool declaration");
            }
            if let Some(d) = t["function"].get("description") {
                string(d)?;
            }
        }
    }
    let messages = array(&r["messages"])?;
    if messages.is_empty() {
        return Err("messages required");
    }
    let mut pending = HashSet::new();
    let mut seen = HashSet::new();
    let mut started = false;
    for m in messages {
        keys(
            m,
            &[
                "role",
                "content",
                "tool_calls",
                "tool_call_id",
                "is_error",
                "reasoning_content",
                "reasoning",
            ],
        )?;
        let role = string(&m["role"])?;
        for key in ["reasoning_content", "reasoning"] {
            if let Some(value) = m.get(key) {
                if role != "assistant" {
                    return Err("assistant-only reasoning");
                }
                string(value)?;
            }
        }
        if let (Some(content), Some(alias)) = (m.get("reasoning_content"), m.get("reasoning"))
            && content != alias
        {
            return Err("conflicting reasoning fields");
        }
        if role == "system" {
            if started || m.as_object().unwrap().len() != 2 {
                return Err("system messages must precede history");
            }
            string(&m["content"])?;
            continue;
        }
        started = true;
        if !matches!(role, "user" | "assistant" | "tool") {
            return Err("unsupported role");
        }
        if !m["content"].is_string()
            && !(role == "assistant" && m["content"].is_null() && m.get("tool_calls").is_some())
        {
            return Err("only text content supported");
        }
        if role != "assistant" && m.get("tool_calls").is_some() {
            return Err("assistant-only calls");
        }
        if role != "tool" && (m.get("tool_call_id").is_some() || m.get("is_error").is_some()) {
            return Err("tool-only fields");
        }
        if role != "tool" && !pending.is_empty() {
            return Err("unresolved tool calls");
        }
        if role == "tool" {
            if !pending.remove(nonempty(&m["tool_call_id"])?) {
                return Err("unknown or duplicate tool result");
            }
            if m.get("is_error").is_some_and(|v| !v.is_boolean()) {
                return Err("invalid tool error flag");
            }
        }
        if let Some(calls) = m.get("tool_calls") {
            for c in array(calls)? {
                keys(c, &["id", "type", "function", "index"])?;
                if c.get("index")
                    .is_some_and(|index| !index.as_u64().is_some_and(|n| n < 32))
                {
                    return Err("invalid tool index");
                }
                keys(&c["function"], &["name", "arguments"])?;
                let id = nonempty(&c["id"])?;
                if c["type"] != "function" || !seen.insert(id) {
                    return Err("invalid/duplicate call");
                }
                nonempty(&c["function"]["name"])?;
                object_json(&c["function"]["arguments"])?;
                pending.insert(id);
            }
        }
    }
    if !started || !pending.is_empty() {
        return Err("incomplete history");
    }
    Ok(names)
}
fn vint(mut n: u64, out: &mut Vec<u8>) {
    while n >= 128 {
        out.push(n as u8 | 128);
        n >>= 7;
    }
    out.push(n as u8);
}
fn v(out: &mut Vec<u8>, field: u64, n: u64) {
    vint(field << 3, out);
    vint(n, out);
}
fn b(out: &mut Vec<u8>, field: u64, bytes: &[u8]) {
    vint(field << 3 | 2, out);
    vint(bytes.len() as u64, out);
    out.extend(bytes);
}
fn s(out: &mut Vec<u8>, field: u64, text: &str) {
    b(out, field, text.as_bytes());
}
fn fixed(out: &mut Vec<u8>, field: u64, n: f64) {
    vint(field << 3 | 1, out);
    out.extend(n.to_le_bytes());
}
fn metadata(c: &Credentials, session: &str, trigger: &str, id: u64, jwt: Option<&str>) -> Vec<u8> {
    let mut out = Vec::new();
    let os = if cfg!(target_os = "macos") {
        "darwin"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else {
        "linux"
    };
    for (f, text) in [
        (1, "devin-desktop"),
        (2, c.version.as_str()),
        (3, c.api_key.as_str()),
        (4, "en"),
        (5, os),
        (7, c.version.as_str()),
        (10, session),
        (12, "devin-desktop"),
        (25, trigger),
        (26, "Unset"),
        (28, "devin-desktop"),
    ] {
        s(&mut out, f, text);
    }
    v(&mut out, 9, id);
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    let mut timestamp = Vec::new();
    v(&mut timestamp, 1, now.as_secs());
    if jwt.is_none() {
        v(&mut timestamp, 2, now.subsec_nanos() as u64);
    }
    b(&mut out, 16, &timestamp);
    if let Some(jwt) = jwt {
        s(&mut out, 21, jwt);
    }
    out
}
fn encode(r: &Value, meta: &[u8], session: &str) -> Vec<u8> {
    let mut out = Vec::new();
    b(&mut out, 1, meta);
    let messages = r["messages"].as_array().unwrap();
    let system: Vec<_> = messages
        .iter()
        .filter(|m| m["role"] == "system")
        .map(|m| m["content"].as_str().unwrap())
        .collect();
    if !system.is_empty() {
        s(&mut out, 2, &system.join("\n\n"));
    }
    for m in messages.iter().filter(|m| m["role"] != "system") {
        let mut p = Vec::new();
        v(
            &mut p,
            2,
            match m["role"].as_str().unwrap() {
                "user" => 1,
                "assistant" => 2,
                _ => 4,
            },
        );
        let text = m["content"].as_str().unwrap_or("");
        s(&mut p, 3, text);
        if let Some(reasoning) = m.get("reasoning_content").or_else(|| m.get("reasoning")) {
            s(&mut p, 11, reasoning.as_str().unwrap());
        }
        v(
            &mut p,
            4,
            ((1 + text.encode_utf16().count()) / 4).max(1) as u64,
        );
        v(&mut p, 5, 1);
        if m["is_error"] == true {
            v(&mut p, 9, 1);
        }
        if let Some(id) = m["tool_call_id"].as_str() {
            s(&mut p, 7, id);
        }
        if let Some(calls) = m["tool_calls"].as_array() {
            for c in calls {
                let mut call = Vec::new();
                s(&mut call, 1, c["id"].as_str().unwrap());
                s(&mut call, 2, c["function"]["name"].as_str().unwrap());
                s(&mut call, 3, c["function"]["arguments"].as_str().unwrap());
                b(&mut p, 6, &call);
            }
        }
        b(&mut out, 3, &p);
    }
    v(&mut out, 7, 5);
    let mut options = Vec::new();
    v(&mut options, 1, 1);
    v(&mut options, 2, r["max_tokens"].as_u64().unwrap_or(4096));
    v(&mut options, 3, 400);
    fixed(&mut options, 5, 1.0);
    v(&mut options, 7, 40);
    fixed(&mut options, 8, 0.95);
    b(&mut out, 8, &options);
    if let Some(tools) = r["tools"].as_array() {
        for t in tools {
            let f = &t["function"];
            let mut tool = Vec::new();
            s(&mut tool, 1, f["name"].as_str().unwrap());
            let description = f["description"].as_str().unwrap_or("");
            let mut shortened = String::new();
            let mut units = 0;
            if description.encode_utf16().count() + 1 > 6998 {
                for c in description.chars() {
                    if units + c.len_utf16() > 6995 {
                        break;
                    }
                    units += c.len_utf16();
                    shortened.push(c);
                }
                shortened.push_str("...");
            }
            s(
                &mut tool,
                2,
                if shortened.is_empty() {
                    description
                } else {
                    &shortened
                },
            );
            s(&mut tool, 3, &f["parameters"].to_string());
            b(&mut out, 10, &tool);
        }
    }
    let mut trajectory = Vec::new();
    s(&mut trajectory, 1, &format!("{session}-trajectory"));
    v(&mut trajectory, 3, 4);
    v(&mut trajectory, 4, 14);
    b(&mut out, 15, &trajectory);
    s(&mut out, 16, &format!("{session}-cascade"));
    v(&mut out, 20, 1);
    s(&mut out, 21, MODEL);
    out
}
fn headers(client: &reqwest::Client, url: &str, unary: bool) -> reqwest::RequestBuilder {
    client
        .post(url)
        .header(
            "content-type",
            if unary {
                "application/proto"
            } else {
                "application/connect+proto"
            },
        )
        .header("Connect-Protocol-Version", "1")
        .header("Connect-Content-Encoding", "identity")
        .header("Connect-Accept-Encoding", "identity")
        .header("Accept-Encoding", "identity")
}
fn check_response(r: &reqwest::Response, expected: &str) -> Result<(), &'static str> {
    if r.status() != reqwest::StatusCode::OK
        || r.headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            != Some(expected)
        || r.headers()
            .get("content-encoding")
            .is_some_and(|v| v != "identity")
    {
        return Err("unexpected live response status/encoding");
    }
    Ok(())
}
pub async fn send(
    client: &reqwest::Client,
    c: &Credentials,
    r: &Value,
) -> Result<reqwest::Response, &'static str> {
    let id = REQUEST.fetch_add(1, Ordering::Relaxed);
    let fresh = format!(
        "helper-{}-{id}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let session = r["user"].as_str().unwrap_or(&fresh);
    let jwt = tokio::time::timeout(Duration::from_secs(30), async {
        let mut request = Vec::new();
        b(&mut request, 1, &metadata(c, session, &fresh, id, None));
        let response = headers(client, AUTH, true)
            .body(request)
            .send()
            .await
            .map_err(|_| "auth connection failed")?;
        check_response(&response, "application/proto")?;
        let mut stream = response.bytes_stream();
        let mut body = Vec::new();
        while let Some(part) = stream.next().await {
            let part = part.map_err(|_| "auth read failed")?;
            if body.len() + part.len() > 65536 {
                return Err("auth response limit");
            }
            body.extend(part);
        }
        let mut jwt = None;
        for (n, f) in fields(&body)? {
            if n == 1 {
                let Field::Bytes(bytes) = f else {
                    return Err("invalid JWT wire type");
                };
                if jwt.replace(utf8(bytes)?).is_some() {
                    return Err("duplicate JWT");
                }
            }
        }
        let jwt = jwt.ok_or("missing JWT")?;
        if jwt.is_empty() || jwt.len() > 65536 || jwt.chars().any(char::is_control) {
            return Err("invalid JWT");
        }
        validate_jwt(&jwt)?;
        Ok(jwt)
    })
    .await
    .map_err(|_| "auth deadline exceeded")??;
    let payload = encode(r, &metadata(c, session, &fresh, id, Some(&jwt)), session);
    if payload.len() > MAX_WIRE {
        return Err("encoded input limit");
    }
    let mut body = vec![0];
    body.extend((payload.len() as u32).to_be_bytes());
    body.extend(payload);
    headers(client, CHAT, false)
        .body(body)
        .send()
        .await
        .map_err(|_| "chat connection failed")
}
fn validate_jwt(jwt: &str) -> Result<(), &'static str> {
    let parts: Vec<_> = jwt.split('.').collect();
    if parts.len() != 3
        || parts.iter().any(|p| {
            p.is_empty()
                || !p
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        })
    {
        return Err("invalid JWT shape");
    }
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(parts[1])
        .map_err(|_| "invalid JWT payload")?;
    let payload: Value = serde_json::from_slice(&bytes).map_err(|_| "invalid JWT payload")?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    if !payload["exp"].as_u64().is_some_and(|exp| exp > now + 30) {
        return Err("invalid JWT expiry");
    }
    Ok(())
}
pub fn chunk(delta: Value, finish: Value) -> String {
    sse(
        json!({"id":"chatcmpl-devin","object":"chat.completion.chunk","created":0,"model":MODEL,
        "choices":[{"index":0,"delta":delta,"finish_reason":finish}]}),
    )
}
pub fn valid_trailer(v: &Value) -> bool {
    v.as_object().is_some_and(|o| {
        !o.contains_key("error")
            && o.get("metadata").is_none_or(|m| {
                m.as_object().is_some_and(|m| {
                    m.values()
                        .all(|v| v.as_array().is_some_and(|a| a.iter().all(Value::is_string)))
                })
            })
    })
}
pub fn response_field(n: u64, f: Field<'_>) -> Result<(), &'static str> {
    match (n, f) {
        (9 | 10 | 21, Field::Bytes(b)) => {
            if !utf8(b)?.is_empty() {
                return Err(match n {
                    9 => "nonempty reasoning unsupported",
                    10 => "nonempty signature unsupported",
                    _ => "nonempty signature type unsupported",
                });
            }
        }
        (11, Field::Varint(0)) => {}
        (9 | 10 | 21 | 11 | 3 | 5 | 6, _) => {
            return Err("unsupported reasoning or incorrect wire type");
        }
        (7, Field::Bytes(b)) => usage(b, false)?,
        (28, Field::Bytes(b)) => usage(b, true)?,
        (7 | 28, _) => return Err("incorrect usage wire type"),
        _ => {}
    }
    Ok(())
}
pub fn read_usage(bytes: &[u8], metrics: bool) -> Result<Vec<(&'static str, u64)>, &'static str> {
    usage(bytes, metrics)?;
    let mut out = Vec::new();
    for (n, f) in fields(bytes)? {
        if !metrics {
            if let (2 | 3, Field::Varint(v)) = (n, f) {
                out.push((
                    if n == 2 {
                        "prompt_tokens"
                    } else {
                        "completion_tokens"
                    },
                    v,
                ));
            }
        } else if let (2, Field::Bytes(b)) = (n, f) {
            let mut metric = None;
            let mut value = None;
            for (n, f) in fields(b)? {
                match (n, f) {
                    (5, Field::Bytes(b)) => metric = Some(utf8(b)?),
                    (4, Field::Bytes(b)) => {
                        for (n, f) in fields(b)? {
                            if let (2, Field::Fixed(b)) = (n, f) {
                                value = Some(
                                    f32::from_le_bytes(
                                        b.try_into().map_err(|_| "incorrect float width")?,
                                    )
                                    .round() as u64,
                                );
                            }
                        }
                    }
                    _ => {}
                }
            }
            if let (Some(metric), Some(value)) = (metric, value)
                && let Some(key) = match metric.as_str() {
                    "input_tokens" => Some("prompt_tokens"),
                    "output_tokens" => Some("completion_tokens"),
                    _ => None,
                }
            {
                out.push((key, value));
            }
        }
    }
    Ok(out)
}
fn usage(bytes: &[u8], metrics: bool) -> Result<(), &'static str> {
    for (n, f) in fields(bytes)? {
        if !metrics && matches!(n, 2..=5) {
            if !matches!(f, Field::Varint(_)) {
                return Err("incorrect usage wire type");
            }
        } else if metrics && n == 2 {
            let Field::Bytes(b) = f else {
                return Err("incorrect metrics wire type");
            };
            for (n, f) in fields(b)? {
                match (n, f) {
                    (5, Field::Bytes(b)) => {
                        utf8(b)?;
                    }
                    (4, Field::Bytes(b)) => {
                        for (n, f) in fields(b)? {
                            if n == 2 {
                                let Field::Fixed(b) = f else {
                                    return Err("incorrect metric value type");
                                };
                                if b.len() != 4 {
                                    return Err("incorrect metric width");
                                }
                                let value = f32::from_le_bytes(b.try_into().unwrap());
                                if !value.is_finite() || value.is_sign_negative() {
                                    return Err("invalid metric count");
                                }
                            }
                        }
                    }
                    (4 | 5, _) => return Err("incorrect metric wire type"),
                    _ => {}
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn request() -> Value {
        json!({"model":MODEL,"stream":true,"user":"abc123","messages":[{"role":"system","content":"system"},{"role":"user","content":"BETA_OK"}],"max_tokens":4096})
    }
    #[test]
    fn bootstrap_and_request_guards() {
        assert!(parse_bootstrap(br#"{"api_key":"private","client_version":"1.2.3"}"#).is_ok());
        assert!(parse_bootstrap(br#"{"api_key":"private","client_version":"latest"}"#).is_err());
        assert!(validate(&request()).is_ok());
        for (key, value) in [
            ("model", json!("other")),
            ("max_tokens", json!(4097)),
            ("user", json!("../bad")),
            ("reasoning", json!("signed")),
        ] {
            let mut r = request();
            r[key] = value;
            assert!(validate(&r).is_err());
        }
    }
    #[test]
    fn parallel_tool_calls_only_accepts_default() {
        let mut r = request();
        assert!(validate(&r).is_ok());
        r["parallel_tool_calls"] = json!(true);
        assert!(validate(&r).is_ok());
        for value in [json!(false), json!(null), json!("true"), json!(1)] {
            r["parallel_tool_calls"] = value;
            assert!(validate(&r).is_err());
        }
    }
    #[test]
    fn full_history_null_content_and_stateless_ids() {
        let mut r = request();
        r["messages"].as_array_mut().unwrap().extend([json!({"role":"assistant","content":null,"tool_calls":[{"id":"call","type":"function","function":{"name":"beta_echo","arguments":"{}"}}]}),json!({"role":"tool","tool_call_id":"call","content":"ok"}),json!({"role":"user","content":"BETA_TOOL_OK"})]);
        assert!(validate(&r).is_ok());
        let a = encode(&r, b"meta", "abc123");
        let b = encode(&r, b"meta", "abc123");
        assert_eq!(a, b);
        assert_ne!(a, encode(&r, b"meta", "other"));
        assert_eq!(
            fields(&a).unwrap().iter().filter(|(n, _)| *n == 3).count(),
            4
        );
        r["messages"][2]["devin-thinking"] = json!({"signature":"secret"});
        assert!(validate(&r).is_err());
    }
    #[test]
    fn unsigned_reasoning_replay_preserves_full_history() {
        for key in ["reasoning_content", "reasoning"] {
            let mut r = request();
            r["messages"].as_array_mut().unwrap().extend([
                json!({"role":"assistant","content":"answer",key:"think 🦀","tool_calls":[{"id":"call","type":"function","function":{"name":"beta_echo","arguments":"{}"}}]}),
                json!({"role":"tool","tool_call_id":"call","content":"ok"}),
                json!({"role":"user","content":"next"}),
                json!({"role":"assistant","content":"last"}),
            ]);
            r["messages"][2]["tool_calls"][0]["index"] = json!(0);
            validate(&r).unwrap();
            r["messages"][2]["tool_calls"][0]["index"] = json!(32);
            assert!(validate(&r).is_err());
            r["messages"][2]["tool_calls"][0]["index"] = json!(0);
            let wire = encode(&r, b"meta", "abc123");
            let messages: Vec<_> = fields(&wire)
                .unwrap()
                .into_iter()
                .filter_map(|(n, f)| {
                    if let (3, Field::Bytes(b)) = (n, f) {
                        Some(fields(b).unwrap())
                    } else {
                        None
                    }
                })
                .collect();
            for (message, expected) in messages
                .iter()
                .zip(["BETA_OK", "answer", "ok", "next", "last"])
            {
                assert!(message.iter().any(
                    |(n, f)| matches!((n, f), (3, Field::Bytes(b)) if *b == expected.as_bytes())
                ));
                assert!(!message.iter().any(|(n, _)| *n == 12));
            }
            assert_eq!(messages.len(), 5);
            assert!(messages[1].iter().any(
                |(n, f)| matches!((n, f), (11, Field::Bytes(b)) if *b == "think 🦀".as_bytes())
            ));
            r["messages"][1]["signature"] = json!("signed");
            assert!(validate(&r).is_err());
        }
    }
    #[test]
    fn reasoning_history_guards() {
        for key in ["reasoning_content", "reasoning"] {
            for value in [json!(null), json!(1), json!({"text":"thinking"})] {
                let mut r = request();
                r["messages"]
                    .as_array_mut()
                    .unwrap()
                    .push(json!({"role":"assistant","content":"answer",key:value}));
                assert!(validate(&r).is_err());
            }
            let mut r = request();
            r["messages"][1][key] = json!("not assistant");
            assert!(validate(&r).is_err());
        }
        let mut r = request();
        r["messages"].as_array_mut().unwrap().push(
            json!({"role":"assistant","content":"answer","reasoning_content":"a","reasoning":"b"}),
        );
        assert!(validate(&r).is_err());
        r["messages"][2]["reasoning"] = json!("a");
        assert!(validate(&r).is_ok());
    }
    #[test]
    fn same_frame_finish_tool_empty_reasoning_metadata_and_usage() {
        let mut payload = Vec::new();
        v(&mut payload, 5, 10);
        let mut call = Vec::new();
        s(&mut call, 1, "id");
        s(&mut call, 2, "beta_echo");
        s(&mut call, 3, "{}");
        b(&mut payload, 6, &call);
        s(&mut payload, 9, "");
        let mut stats = Vec::new();
        v(&mut stats, 2, 10);
        b(&mut payload, 7, &stats);
        let mut d = Decoder::new(HashSet::from(["beta_echo".into()]));
        d.live = true;
        let mut frame = vec![0];
        frame.extend((payload.len() as u32).to_be_bytes());
        frame.extend(payload);
        assert!(d.push(&frame).is_ok());
        let trailer = br#"{"metadata":{"x":["y"]}}"#;
        let mut eos = vec![2];
        eos.extend((trailer.len() as u32).to_be_bytes());
        eos.extend(trailer);
        d.push(&eos).unwrap();
        assert_eq!(d.complete().unwrap().len(), 1);
        assert!(response_field(9, Field::Bytes(b"thinking")).is_err());
        assert!(response_field(10, Field::Bytes(b"signature")).is_err());
        assert!(response_field(11, Field::Varint(1)).is_err());
        assert!(response_field(7, Field::Varint(1)).is_err());
        assert!(response_field(7, Field::Bytes(&[18, 0])).is_err());
        assert!(response_field(28, Field::Bytes(&[16, 0])).is_err());
        assert!(!valid_trailer(&json!({"error":{"code":"bad"}})));
        assert!(!valid_trailer(&json!({"metadata":{"x":"bad"}})));
    }
}
