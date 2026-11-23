use super::*;
use base64::Engine;
use std::io::{BufRead, Read};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

pub const CHAT: &str =
    "https://server.codeium.com/exa.api_server_pb.ApiServerService/GetChatMessage";
const AUTH: &str = "https://server.codeium.com/exa.auth_pb.AuthService/GetUserJwt";
const MODELS: &str =
    "https://server.codeium.com/exa.api_server_pb.ApiServerService/GetCliModelConfigs";
const MAX_CATALOG: usize = 8 * 1024 * 1024;
const QUOTA: &str =
    "https://server.codeium.com/exa.seat_management_pb.SeatManagementService/GetUserStatus";
const MAX_QUOTA: usize = 1024 * 1024;
pub const DEADLINE: Duration = Duration::from_secs(120);
static REQUEST: AtomicU64 = AtomicU64::new(1);

pub struct Credentials {
    api_key: String,
    version: String,
}

fn discovery_request(c: &Credentials) -> Vec<u8> {
    let mut meta = Vec::new();
    for (field, text) in [
        (1, "chisel"),
        (12, "chisel"),
        (2, "0.0.0-dev"),
        (7, "0.0.0-dev"),
        (3, c.api_key.as_str()),
        (4, "en"),
        (
            5,
            if cfg!(target_os = "macos") {
                "darwin"
            } else if cfg!(target_os = "windows") {
                "windows"
            } else {
                "linux"
            },
        ),
    ] {
        s(&mut meta, field, text);
    }
    b(&mut meta, 30, &[3, 4, 6, 7, 8]);
    let mut out = Vec::new();
    b(&mut out, 1, &meta);
    out
}

pub async fn fetch_models(
    client: &reqwest::Client,
    c: &Credentials,
) -> Result<Vec<Value>, &'static str> {
    read_models(headers(client, MODELS, true).body(discovery_request(c))).await
}

async fn read_models(request: reqwest::RequestBuilder) -> Result<Vec<Value>, &'static str> {
    tokio::time::timeout(Duration::from_secs(30), async {
        let response = request
            .send()
            .await
            .map_err(|_| "catalog connection failed")?;
        check_response(&response, "application/proto")?;
        if response
            .content_length()
            .is_some_and(|n| n > MAX_CATALOG as u64)
        {
            return Err("catalog response limit");
        }
        let mut stream = response.bytes_stream();
        let mut body = Vec::new();
        while let Some(part) = stream.next().await {
            let part = part.map_err(|_| "catalog read failed")?;
            if part.len() > MAX_CATALOG - body.len() {
                return Err("catalog response limit");
            }
            body.extend(part);
        }
        decode_models(&body)
    })
    .await
    .map_err(|_| "catalog deadline exceeded")?
}

pub async fn fetch_quota(client: &reqwest::Client, c: &Credentials) -> Result<Value, &'static str> {
    let os = if cfg!(target_os = "macos") {
        "darwin"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else {
        "linux"
    };
    let body = json!({"metadata":{"api_key":c.api_key,"ide_name":"devin-desktop",
        "ide_version":c.version,"extension_name":"devin-desktop",
        "extension_version":c.version,"locale":"en","os":os}});
    read_quota(
        client
            .post(QUOTA)
            .header("content-type", "application/json")
            .header("Connect-Protocol-Version", "1")
            .header("Accept-Encoding", "identity")
            .bearer_auth(&c.api_key)
            .body(body.to_string()),
    )
    .await
}

async fn read_quota(request: reqwest::RequestBuilder) -> Result<Value, &'static str> {
    tokio::time::timeout(Duration::from_secs(15), async {
        let response = request
            .send()
            .await
            .map_err(|_| "quota connection failed")?;
        if matches!(response.status().as_u16(), 401 | 403) {
            return Err("quota unavailable for this account");
        }
        check_response(&response, "application/json")?;
        let mut stream = response.bytes_stream();
        let mut body = Vec::new();
        while let Some(part) = stream.next().await {
            let part = part.map_err(|_| "quota read failed")?;
            if part.len() > MAX_QUOTA - body.len() {
                return Err("quota response limit");
            }
            body.extend(part);
        }
        decode_quota(&serde_json::from_slice(&body).map_err(|_| "invalid quota response")?)
    })
    .await
    .map_err(|_| "quota deadline exceeded")?
}

pub fn decode_quota(root: &Value) -> Result<Value, &'static str> {
    let get = |v: &Value, camel: &str, snake: &str| -> Value {
        v.get(camel)
            .or_else(|| v.get(snake))
            .cloned()
            .unwrap_or(Value::Null)
    };
    let plan = get(
        &get(root, "userStatus", "user_status"),
        "planStatus",
        "plan_status",
    );
    let mut out = json!({});
    for (window, percent, reset) in [
        (
            "daily",
            (
                "dailyQuotaRemainingPercent",
                "daily_quota_remaining_percent",
            ),
            ("dailyQuotaResetAtUnix", "daily_quota_reset_at_unix"),
        ),
        (
            "weekly",
            (
                "weeklyQuotaRemainingPercent",
                "weekly_quota_remaining_percent",
            ),
            ("weeklyQuotaResetAtUnix", "weekly_quota_reset_at_unix"),
        ),
    ] {
        let remaining = get(&plan, percent.0, percent.1);
        if remaining.is_null() {
            continue;
        }
        let remaining = remaining
            .as_f64()
            .filter(|p| (0.0..=100.0).contains(p))
            .ok_or("invalid quota percentage")?;
        out[window] = json!({"remaining_percent": remaining});
        let reset = get(&plan, reset.0, reset.1);
        let reset = match &reset {
            Value::Null => None,
            Value::String(s) => Some(s.parse::<u64>().map_err(|_| "invalid quota reset")?),
            v => Some(v.as_u64().ok_or("invalid quota reset")?),
        };
        if let Some(reset) = reset.filter(|r| *r > 0) {
            out[window]["reset_at"] = json!(reset);
        }
    }
    if out.as_object().unwrap().is_empty() {
        return Err("quota not reported for this account");
    }
    Ok(out)
}

// Reject ambiguous consumed singular fields instead of protobuf's last-wins behavior.
fn number(fs: &[(u64, Field<'_>)], field: u64) -> Result<u64, &'static str> {
    let mut value = None;
    for (_, f) in fs.iter().filter(|(n, _)| *n == field) {
        let Field::Varint(v) = f else {
            return Err("invalid catalog number");
        };
        if value.replace(*v).is_some() {
            return Err("duplicate catalog field");
        }
    }
    Ok(value.unwrap_or(0))
}
fn bytes<'a>(fs: &[(u64, Field<'a>)], field: u64) -> Result<&'a [u8], &'static str> {
    let mut value = None;
    for (_, f) in fs.iter().filter(|(n, _)| *n == field) {
        let Field::Bytes(v) = f else {
            return Err("invalid catalog bytes");
        };
        if value.replace(*v).is_some() {
            return Err("duplicate catalog field");
        }
    }
    Ok(value.unwrap_or(&[]))
}
fn flag(fs: &[(u64, Field<'_>)], field: u64) -> Result<bool, &'static str> {
    match number(fs, field)? {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err("invalid catalog boolean"),
    }
}

pub fn decode_models(body: &[u8]) -> Result<Vec<Value>, &'static str> {
    if body.len() > MAX_CATALOG {
        return Err("catalog response limit");
    }
    let mut models = Vec::new();
    let mut seen = HashSet::new();
    for (n, f) in fields_limit(body, 16384)? {
        if n != 1 {
            continue;
        }
        let Field::Bytes(config) = f else {
            return Err("invalid model config");
        };
        let config = fields(config)?;
        let info = fields(bytes(&config, 23)?)?;
        if flag(&config, 4)?
            || flag(&info, 2)?
            || flag(&info, 25)?
            || !matches!(number(&info, 22)?, 0 | 8)
        {
            continue;
        }
        let uid = utf8(bytes(&config, 22)?)?;
        if uid.is_empty()
            || uid.len() > 256
            || uid.chars().any(|c| c.is_control() || c.is_whitespace())
        {
            return Err("invalid catalog UID");
        }
        let info_uid = bytes(&info, 17)?;
        if !info_uid.is_empty() && info_uid != uid.as_bytes() {
            return Err("conflicting catalog UID");
        }
        let override_context = number(&config, 18)?;
        let context = if override_context > 0 {
            override_context
        } else {
            number(&info, 4)?
        };
        let output = number(&info, 13)?;
        if context == 0 || context > i32::MAX as u64 || output == 0 || output > i32::MAX as u64 {
            return Err("invalid catalog token limits");
        }
        let features = fields(bytes(&info, 6)?)?;
        let tools = flag(&features, 12)?;
        let parallel = flag(&features, 21)?;
        let reasoning = flag(&features, 15)?;
        let images = flag(&features, 11)?;
        let preserve = flag(&features, 25)?;
        let interleave = flag(&features, 24)?;
        if !seen.insert(uid.clone()) {
            return Err("duplicate catalog UID");
        }
        let name = utf8(bytes(&config, 1)?)?;
        models.push(json!({"id":uid,"object":"model","owned_by":"devin",
            "name":if name.is_empty() { &uid } else { &name }, "context_window":context,
            "max_output_tokens":output.min(16384),
            "capabilities":{"tool_use":tools,"parallel_tool_calls":parallel,"images":false,
                "reasoning":reasoning,"preserve_reasoning":preserve,"interleaved_reasoning":interleave,
                "upstream_images":images}}));
    }
    if models.is_empty() {
        return Err("empty usable model catalog");
    }
    models.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
    Ok(models)
}

pub fn select_model(r: &mut Value, catalog: &[Value]) -> Result<(), &'static str> {
    let model = catalog
        .iter()
        .find(|m| m["id"] == r["model"])
        .ok_or("unknown or unavailable model UID")?;
    let cap = model["max_output_tokens"]
        .as_u64()
        .ok_or("invalid catalog output limit")?;
    if r["max_tokens"].as_u64().is_some_and(|n| n > cap) {
        return Err("max_tokens exceeds model output limit");
    }
    if r.get("max_tokens").is_none() {
        r["max_tokens"] = json!(cap);
    }
    if r["tools"].as_array().is_some_and(|t| !t.is_empty())
        && model["capabilities"]["tool_use"] != true
    {
        return Err("model does not support tools");
    }
    if model["capabilities"]["parallel_tool_calls"] != true {
        if r["parallel_tool_calls"] == true {
            return Err("model does not support parallel tools");
        }
        r["parallel_tool_calls"] = json!(false);
    }
    Ok(())
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
    let model = nonempty(&r["model"])?;
    if model.len() > 256
        || model.chars().any(|c| c.is_control() || c.is_whitespace())
        || r["stream"] != true
    {
        return Err("a model UID and streaming are required");
    }
    if let Some(n) = r.get("max_tokens")
        && !n.as_u64().is_some_and(|n| (1..=16384).contains(&n))
    {
        return Err("max_tokens must be 1..16384");
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
        .is_some_and(|v| !v.is_boolean())
    {
        return Err("parallel_tool_calls must be boolean");
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
        b(&mut out, 13, &[8, 1]);
    }
    let mut prompts = messages.iter().filter(|m| m["role"] != "system").peekable();
    while let Some(m) = prompts.next() {
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
        if prompts.peek().is_none() {
            b(&mut p, 8, &[8, 1]);
        }
        b(&mut out, 3, &p);
    }
    v(&mut out, 7, 5);
    let mut options = Vec::new();
    v(&mut options, 1, 1);
    v(&mut options, 2, r["max_tokens"].as_u64().unwrap_or(16384));
    v(&mut options, 3, 400);
    fixed(&mut options, 5, 1.0);
    v(&mut options, 7, 40);
    fixed(&mut options, 8, 0.95);
    b(&mut out, 8, &options);
    if r["parallel_tool_calls"] == false {
        v(&mut out, 11, 1);
    }
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
    s(&mut out, 21, r["model"].as_str().unwrap());
    s(&mut out, 27, session);
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
pub fn chunk(model: &str, delta: Value, finish: Value) -> String {
    sse(
        json!({"id":"chatcmpl-devin","object":"chat.completion.chunk","created":0,"model":model,
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
        (20, Field::Bytes(b)) => {
            if !b.is_empty() {
                return Err("nonempty Gemini signature unsupported");
            }
        }
        (11, Field::Varint(0)) => {}
        (9 | 10 | 20 | 21 | 11 | 3 | 5 | 6, _) => {
            return Err("unsupported reasoning or incorrect wire type");
        }
        (7, Field::Bytes(b)) => usage(b, false)?,
        (28, Field::Bytes(b)) => usage(b, true)?,
        (7 | 28, _) => return Err("incorrect usage wire type"),
        _ => {}
    }
    Ok(())
}
fn token_metric(name: &str) -> Option<&'static str> {
    match name {
        "input_tokens" => Some("input_tokens"),
        "output_tokens" => Some("output_tokens"),
        "cache_write_tokens" | "cache_creation_tokens" => Some("cache_write_tokens"),
        "cache_read_tokens" | "cached_tokens" => Some("cache_read_tokens"),
        _ => None,
    }
}
pub fn read_usage(bytes: &[u8], metrics: bool) -> Result<Vec<(&'static str, u64)>, &'static str> {
    usage(bytes, metrics)?;
    let mut out = Vec::new();
    for (n, f) in fields(bytes)? {
        if !metrics {
            if let (2..=5, Field::Varint(v)) = (n, f) {
                out.push((
                    match n {
                        2 => "input_tokens",
                        3 => "output_tokens",
                        4 => "cache_write_tokens",
                        _ => "cache_read_tokens",
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
                                value = Some(f32::from_le_bytes(
                                    b.try_into().map_err(|_| "incorrect float width")?,
                                ) as u64);
                            }
                        }
                    }
                    _ => {}
                }
            }
            if let (Some(metric), Some(value)) = (metric, value)
                && let Some(key) = token_metric(&metric)
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
            let fs = fields(b)?;
            let mut token = false;
            for (n, f) in &fs {
                if let (5, Field::Bytes(b)) = (n, f) {
                    token |= token_metric(&utf8(b)?).is_some();
                }
            }
            for (n, f) in fs {
                match (n, f) {
                    (5, Field::Bytes(_)) => {}
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
                                if !value.is_finite()
                                    || value.is_sign_negative()
                                    || (token
                                        && (value.fract() != 0.0
                                            || value >= 18446744073709551616.0))
                                {
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
    fn config(uid: &str, disabled: bool, slot: u64, context: u64, output: u64) -> Vec<u8> {
        let mut features = Vec::new();
        for field in [11, 12, 15, 21, 24, 25] {
            v(&mut features, field, 1);
        }
        let mut info = Vec::new();
        v(&mut info, 4, context);
        v(&mut info, 13, output);
        v(&mut info, 22, slot);
        b(&mut info, 6, &features);
        let mut config = Vec::new();
        s(&mut config, 22, uid);
        s(&mut config, 1, "Display name");
        v(&mut config, 4, disabled as u64);
        b(&mut config, 23, &info);
        config
    }
    fn catalog_wire(configs: &[Vec<u8>]) -> Vec<u8> {
        let mut body = Vec::new();
        for config in configs {
            b(&mut body, 1, config);
        }
        body
    }
    #[test]
    fn catalog_schema_filter_and_capabilities() {
        let mut configs = vec![config("hidden", true, 0, 0, 0); 1357];
        for slot in [3, 4, 6, 7] {
            configs.push(config("hidden", false, slot, 0, 0));
        }
        configs.extend([
            config("one", false, 0, 200000, 64000),
            config("two", false, 8, 1000000, 2048),
        ]);
        let models = decode_models(&catalog_wire(&configs)).unwrap();
        assert_eq!(models.len(), 2);
        assert_eq!(models[0]["context_window"], 200000);
        assert_eq!(models[0]["max_output_tokens"], 16384);
        assert_eq!(models[1]["max_output_tokens"], 2048);
        assert_eq!(models[0]["capabilities"]["images"], false);
        for key in [
            "tool_use",
            "parallel_tool_calls",
            "reasoning",
            "preserve_reasoning",
            "interleaved_reasoning",
            "upstream_images",
        ] {
            assert_eq!(models[0]["capabilities"][key], true);
        }
        let mut override_config = config("override", false, 0, 100, 50);
        v(&mut override_config, 18, 1000);
        assert_eq!(
            decode_models(&catalog_wire(&[override_config])).unwrap()[0]["context_window"],
            1000
        );
    }
    #[test]
    fn catalog_rejects_malformed_ambiguous_and_unbounded_data() {
        for body in [
            vec![],
            vec![10, 255],
            vec![8, 1],
            vec![0; MAX_CATALOG + 1],
            catalog_wire(&vec![config("duplicate", false, 0, 100, 50); 2]),
            catalog_wire(&[config("bad uid", false, 0, 100, 50)]),
            catalog_wire(&[config("missing-context", false, 0, 0, 50)]),
            catalog_wire(&[config("missing-output", false, 0, 100, 0)]),
            catalog_wire(&[config("negative", false, 0, u64::MAX, 50)]),
        ] {
            assert!(decode_models(&body).is_err());
        }
        for field in [22, 18] {
            let mut bad = config("one", false, 0, 100, 50);
            s(&mut bad, field, "bad");
            assert!(decode_models(&catalog_wire(&[bad])).is_err());
        }
        for field in [2, 25] {
            let mut info = Vec::new();
            v(&mut info, field, 1);
            let mut hidden = Vec::new();
            s(&mut hidden, 22, "hidden");
            b(&mut hidden, 23, &info);
            assert!(decode_models(&catalog_wire(&[hidden])).is_err());
        }
    }
    #[test]
    fn two_models_validate_encode_and_stream_exact_uid() {
        let models = decode_models(&catalog_wire(&[
            config("one", false, 0, 10000, 5000),
            config("two", false, 8, 20000, 1024),
        ]))
        .unwrap();
        for uid in ["one", "two"] {
            let mut r = request();
            r["model"] = json!(uid);
            r.as_object_mut().unwrap().remove("max_tokens");
            validate(&r).unwrap();
            select_model(&mut r, &models).unwrap();
            let wire = encode(&r, b"meta", "session");
            assert_eq!(bytes(&fields(&wire).unwrap(), 21).unwrap(), uid.as_bytes());
            assert!(
                chunk(uid, json!({"content":"ok"}), Value::Null)
                    .contains(&format!("\"model\":\"{uid}\""))
            );
        }
        let mut r = request();
        r["model"] = json!("unknown");
        assert!(select_model(&mut r, &models).is_err());
        r["model"] = json!("two");
        assert!(select_model(&mut r, &models).is_err());
    }
    #[test]
    fn discovery_metadata_uses_portable_fixed_identity() {
        let c = parse_bootstrap(br#"{"api_key":"synthetic","client_version":"1.2.3"}"#).unwrap();
        let wire = discovery_request(&c);
        let outer = fields(&wire).unwrap();
        let meta = fields(bytes(&outer, 1).unwrap()).unwrap();
        for field in [1, 12] {
            assert_eq!(bytes(&meta, field).unwrap(), b"chisel");
        }
        for field in [2, 7] {
            assert_eq!(bytes(&meta, field).unwrap(), b"0.0.0-dev");
        }
        assert_eq!(bytes(&meta, 3).unwrap(), b"synthetic");
        assert_eq!(bytes(&meta, 30).unwrap(), &[3, 4, 6, 7, 8]);
    }
    #[tokio::test]
    async fn catalog_transport_is_offline_bounded_and_strict() {
        for (status, content_type, body) in [
            (
                200,
                "application/proto",
                catalog_wire(&[config("one", false, 0, 1000, 100)]),
            ),
            (302, "application/proto", vec![]),
            (200, "application/json", vec![]),
            (200, "application/proto", vec![0; MAX_CATALOG + 1]),
        ] {
            let expected =
                status == 200 && content_type == "application/proto" && body.len() < MAX_CATALOG;
            let app = Router::new().route(
                "/",
                post(move || async move {
                    Response::builder()
                        .status(status)
                        .header("content-type", content_type)
                        .body(Body::from(body))
                        .unwrap()
                }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let client = reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap();
            assert_eq!(
                read_models(client.post(format!("http://{address}/")))
                    .await
                    .is_ok(),
                expected
            );
            server.abort();
        }
    }
    #[test]
    fn quota_decodes_only_reported_windows() {
        let camel = json!({"userStatus":{"email":"private","planStatus":{
            "dailyQuotaRemainingPercent":62.5,"dailyQuotaResetAtUnix":"1800000000",
            "weeklyQuotaRemainingPercent":100}}});
        assert_eq!(
            decode_quota(&camel).unwrap(),
            json!({"daily":{"remaining_percent":62.5,"reset_at":1800000000_u64},
                   "weekly":{"remaining_percent":100.0}})
        );
        let snake = json!({"user_status":{"plan_status":{
            "weekly_quota_remaining_percent":0,"weekly_quota_reset_at_unix":1800000001_u64}}});
        assert_eq!(
            decode_quota(&snake).unwrap(),
            json!({"weekly":{"remaining_percent":0.0,"reset_at":1800000001_u64}})
        );
        for bad in [
            json!({}),
            json!({"userStatus":{"planStatus":{}}}),
            json!({"userStatus":{"planStatus":{"dailyQuotaRemainingPercent":101}}}),
            json!({"userStatus":{"planStatus":{"dailyQuotaRemainingPercent":-1}}}),
            json!({"userStatus":{"planStatus":{"dailyQuotaRemainingPercent":"50"}}}),
            json!({"userStatus":{"planStatus":{"dailyQuotaRemainingPercent":5,
                "dailyQuotaResetAtUnix":"soon"}}}),
        ] {
            assert!(decode_quota(&bad).is_err());
        }
    }
    #[tokio::test]
    async fn quota_transport_is_bounded_and_strict() {
        let good = br#"{"userStatus":{"planStatus":{"dailyQuotaRemainingPercent":40}}}"#;
        for (status, content_type, body, expected) in [
            (200, "application/json", good.to_vec(), Ok(())),
            (
                403,
                "application/json",
                vec![],
                Err("quota unavailable for this account"),
            ),
            (
                302,
                "application/json",
                vec![],
                Err("unexpected live response status/encoding"),
            ),
            (
                200,
                "text/html",
                good.to_vec(),
                Err("unexpected live response status/encoding"),
            ),
            (
                200,
                "application/json",
                b"{".to_vec(),
                Err("invalid quota response"),
            ),
            (
                200,
                "application/json",
                vec![b' '; MAX_QUOTA + 1],
                Err("quota response limit"),
            ),
        ] {
            let app = Router::new().route(
                "/",
                post(move || async move {
                    Response::builder()
                        .status(status)
                        .header("content-type", content_type)
                        .body(Body::from(body))
                        .unwrap()
                }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let client = reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap();
            assert_eq!(
                read_quota(client.post(format!("http://{address}/")))
                    .await
                    .map(|_| ()),
                expected
            );
            server.abort();
        }
    }
    #[tokio::test]
    async fn chat_structure_precedes_discovery_and_unknown_models_never_generate() {
        let c = parse_bootstrap(br#"{"api_key":"synthetic","client_version":"1.2.3"}"#).unwrap();
        let app = Arc::new(App {
            key: "x".repeat(32),
            live: Some(c),
            upstream: offline_url("http://127.0.0.1:1").unwrap(),
            client: reqwest::Client::builder().no_proxy().build().unwrap(),
            slots: Arc::new(Semaphore::new(1)),
            catalog: RwLock::new(None),
            quota: RwLock::new(None),
        });
        let send = |value: Value| {
            let mut req = Request::new(Body::from(value.to_string()));
            req.headers_mut().insert(
                "authorization",
                format!("Bearer {}", app.key).parse().unwrap(),
            );
            req.headers_mut()
                .insert("content-type", "application/json".parse().unwrap());
            req
        };
        let malformed = json!({"model":"one","stream":true,"messages":[]});
        let response = tokio::time::timeout(
            Duration::from_secs(1),
            chat(State(app.clone()), send(malformed)),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let models = decode_models(&catalog_wire(&[config("one", false, 0, 10000, 4096)])).unwrap();
        *app.catalog.write().await = Some((std::time::Instant::now(), models));
        let mut r = request();
        r["model"] = json!("unknown");
        assert_eq!(
            chat(State(app.clone()), send(r)).await.status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(app.slots.available_permits(), 1);
    }
    const MODEL: &str = "swe-2-flash-medium";
    fn request() -> Value {
        json!({"model":MODEL,"stream":true,"user":"abc123","messages":[{"role":"system","content":"system"},{"role":"user","content":"BETA_OK"}],"max_tokens":4096})
    }
    #[test]
    fn bootstrap_and_request_guards() {
        assert!(parse_bootstrap(br#"{"api_key":"private","client_version":"1.2.3"}"#).is_ok());
        assert!(parse_bootstrap(br#"{"api_key":"private","client_version":"latest"}"#).is_err());
        assert!(validate(&request()).is_ok());
        for (key, value) in [
            ("model", json!("")),
            ("max_tokens", json!(16385)),
            ("user", json!("../bad")),
            ("reasoning", json!("signed")),
        ] {
            let mut r = request();
            r[key] = value;
            assert!(validate(&r).is_err());
        }
    }
    #[test]
    fn reasoning_output_budget_is_bounded_by_catalog_and_bridge() {
        let models =
            decode_models(&catalog_wire(&[config(MODEL, false, 0, 200000, 64000)])).unwrap();
        let mut r = request();
        r["max_tokens"] = json!(16384);
        assert!(validate(&r).is_ok());
        select_model(&mut r, &models).unwrap();
        assert_eq!(r["max_tokens"], 16384);
        r["max_tokens"] = json!(16385);
        assert!(validate(&r).is_err());
        assert!(select_model(&mut r, &models).is_err());
        r.as_object_mut().unwrap().remove("max_tokens");
        select_model(&mut r, &models).unwrap();
        assert_eq!(r["max_tokens"], 16384);
    }

    #[test]
    fn parallel_tool_calls_accepts_only_booleans() {
        let mut r = request();
        assert!(validate(&r).is_ok());
        r["parallel_tool_calls"] = json!(true);
        assert!(validate(&r).is_ok());
        r["parallel_tool_calls"] = json!(false);
        assert!(validate(&r).is_ok());
        for value in [json!(null), json!("true"), json!(1), json!([]), json!({})] {
            r["parallel_tool_calls"] = value;
            assert!(validate(&r).is_err());
        }
    }
    #[test]
    fn parallel_capability_and_request_encode_serial_mode_once() {
        let mut models =
            decode_models(&catalog_wire(&[config(MODEL, false, 0, 10000, 4096)])).unwrap();
        for supported in [false, true] {
            models[0]["capabilities"]["parallel_tool_calls"] = json!(supported);
            for tools in [
                None,
                Some(json!([])),
                Some(json!([{"type":"function","function":{"name":"echo","parameters":{}}}])),
            ] {
                for parallel in [None, Some(false), Some(true)] {
                    let mut r = request();
                    if let Some(tools) = &tools {
                        r["tools"] = tools.clone();
                    }
                    if let Some(parallel) = parallel {
                        r["parallel_tool_calls"] = json!(parallel);
                    }
                    validate(&r).unwrap();
                    if !supported && parallel == Some(true) {
                        assert_eq!(
                            select_model(&mut r, &models),
                            Err("model does not support parallel tools")
                        );
                        continue;
                    }
                    select_model(&mut r, &models).unwrap();
                    let selected = r.clone();
                    select_model(&mut r, &models).unwrap();
                    assert_eq!(r, selected);
                    let wire = encode(&r, b"meta", "session");
                    assert_eq!(wire, encode(&r, b"meta", "session"));
                    let fs = fields(&wire).unwrap();
                    let disable: Vec<_> = fs.iter().filter(|(n, _)| *n == 11).collect();
                    if !supported || parallel == Some(false) {
                        assert_eq!(disable.len(), 1);
                        assert!(matches!(disable[0], (11, Field::Varint(1))));
                    } else {
                        assert!(disable.is_empty());
                    }
                    assert_eq!(
                        fs.iter().filter(|(n, _)| *n == 10).count(),
                        usize::from(
                            tools
                                .as_ref()
                                .is_some_and(|t| !t.as_array().unwrap().is_empty())
                        )
                    );
                    let options = fields(bytes(&fs, 8).unwrap()).unwrap();
                    assert!(!options.iter().any(|(n, _)| *n == 11));
                }
            }
        }
    }
    #[test]
    fn gemini_signature_bytes_and_wire_types_fail_closed() {
        assert!(response_field(20, Field::Bytes(b"")).is_ok());
        for bytes in [b"signed".as_slice(), &[0xff, 0x00]] {
            assert_eq!(
                response_field(20, Field::Bytes(bytes)),
                Err("nonempty Gemini signature unsupported")
            );
        }
        for field in [
            Field::Varint(0),
            Field::Varint(1),
            Field::Fixed(&[0; 4]),
            Field::Fixed(&[0; 8]),
        ] {
            assert!(response_field(20, field).is_err());
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
    fn cache_breakpoints_and_session_namespace() {
        for messages in [
            json!([]),
            json!([{"role":"system","content":"sys"}]),
            json!([{"role":"user","content":"first"}]),
            json!([{"role":"system","content":"sys"},{"role":"user","content":"first"},{"role":"assistant","content":"answer"},{"role":"tool","content":"result","tool_call_id":"id"},{"role":"system","content":"tail"}]),
        ] {
            let r = json!({"model":MODEL,"messages":messages});
            let wire = encode(&r, b"meta", "session");
            assert_eq!(wire, encode(&r, b"meta", "session"));
            let fs = fields(&wire).unwrap();
            let system = messages
                .as_array()
                .unwrap()
                .iter()
                .any(|m| m["role"] == "system");
            assert_eq!(
                fs.iter().filter(|(n, _)| *n == 13).count(),
                usize::from(system)
            );
            if system {
                assert_eq!(number(&fields(bytes(&fs, 13).unwrap()).unwrap(), 1), Ok(1));
            }
            let prompts: Vec<_> = fs
                .iter()
                .filter_map(|(n, f)| match (n, f) {
                    (3, Field::Bytes(b)) => Some(fields(b).unwrap()),
                    _ => None,
                })
                .collect();
            for (i, prompt) in prompts.iter().enumerate() {
                assert_eq!(
                    prompt.iter().filter(|(n, _)| *n == 8).count(),
                    usize::from(i + 1 == prompts.len())
                );
                if i + 1 == prompts.len() {
                    assert_eq!(
                        number(&fields(bytes(prompt, 8).unwrap()).unwrap(), 1),
                        Ok(1)
                    );
                }
            }
            assert_eq!(bytes(&fs, 27).unwrap(), b"session");
            let mut appended = r.clone();
            appended["messages"]
                .as_array_mut()
                .unwrap()
                .push(json!({"role":"user","content":"next"}));
            let next_wire = encode(&appended, b"different-meta", "session");
            let next = fields(&next_wire).unwrap();
            let other_wire = encode(&r, b"meta", "other-session");
            let other = fields(&other_wire).unwrap();
            for n in [15, 16, 27] {
                assert_eq!(bytes(&fs, n).unwrap(), bytes(&next, n).unwrap());
                assert_ne!(bytes(&fs, n).unwrap(), bytes(&other, n).unwrap());
            }
        }
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
