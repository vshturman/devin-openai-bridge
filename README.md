# devin-openai-bridge

Unofficial local adapter exposing Devin chat through a subset of the OpenAI Chat
Completions API. Written in Rust; no Emacs dependency.

Experimental and not affiliated with or endorsed by the service provider. Uses
an internal protocol that may change without notice. Verify that your use complies
with the provider's terms. Catalog pricing is not a billing guarantee.

## Build and test

Install stable Rust, then run:

```sh
cargo build --release --locked
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
cargo fmt --check
```

The executable is `target/release/devin-helper` (`devin-helper.exe` on Windows).
Tests use synthetic local servers and do not need provider credentials.

Tagged releases provide Linux x64/ARM64, macOS Intel/Apple Silicon, and Windows
x64 archives plus `SHA256SUMS`. Linux binaries are built on Ubuntu 24.04 and
require a compatible glibc. Binaries are not signed or notarized.

## Start

1. Set `DEVIN_HELPER_KEY` to a randomly generated local bearer key containing at
   least 32 ASCII alphanumeric characters. Do not reuse your provider API key.
2. Launch `devin-helper --live` with a pipe connected to stdin.
3. Write one JSON object, followed by a newline, to stdin, then close stdin:

```json
{"api_key":"YOUR_EXISTING_PROVIDER_API_KEY","client_version":"YOUR_COMPATIBLE_CLIENT_VERSION"}
```

`client_version` must be a numeric three-part version. Supply an explicitly
chosen compatible version; the helper does not discover an installed client or
read credential files. Chat identifies as `devin-desktop`; discovery uses the
fixed `chisel` / `0.0.0-dev` metadata identity.
The example values above are placeholders, not working credentials.

Read the first stdout line for the allocated `127.0.0.1:PORT` address. Configure
your client with base URL `http://127.0.0.1:PORT/v1`, the local bearer key, and
streaming enabled. Fetch `GET /v1/models` with that bearer key and select an exact
returned `id` as the chat model; labels are not model IDs. Keep the process running for the
client's session; terminate it when finished. There is no fixed port or automatic
restart. Never put provider credentials in command arguments, shell history,
checked-in files, or request logs.

## Supported API

- `POST /v1/chat/completions`, bearer-authenticated, streaming SSE only.
- `GET /v1/models`, bearer-authenticated, returns `{"object":"list","data":[...]}`.
  Each model has `id` (exact upstream UID), `object:"model"`, `owned_by:"devin"`,
  `name`, `context_window`, `max_output_tokens`, and `capabilities`. Capability
  booleans are `tool_use`, `parallel_tool_calls`, `images`, `reasoning`,
  `preserve_reasoning`, `interleaved_reasoning`, and `upstream_images`.
  `images` is always false: upstream image support is informational only, and
  image requests remain unsupported. Reasoning flags describe unsigned history,
  not signed/redacted state support.
- `GET /v1/usage`, bearer-authenticated, returns account quota as
  `{"daily":{"remaining_percent":N,"reset_at":UNIX},"weekly":{...}}`. Only
  windows the provider reports are included; `reset_at` may be absent. Data comes
  from the fixed HTTPS `GetUserStatus` RPC (15-second timeout, 1 MiB bound) and
  is cached for 60 seconds. Accounts without quota access get HTTP 502; chat is
  unaffected.
- Catalogs come from the fixed HTTPS `GetCliModelConfigs` RPC, with a 30-second
  timeout and 8 MiB response bound. Successful results are shared for 15 minutes;
  GET and chat reuse the cache, refreshing lazily on expiry. No CLI or credential
  file lookup is used. Discovery errors, empty catalogs, malformed metadata and
  ambiguous duplicate IDs fail closed; expired data is not a fallback.
- Only enabled public non-router configs in normal display slots (0 or 8), with
  positive context/output limits, are eligible. Chat rejects unknown model IDs
  before generation. Output is limited to the lesser of the model limit and
  16,384 tokens, including reasoning; omitted `max_tokens` uses that capped
  catalog limit. Clients may explicitly request a smaller budget. Token-limit
  stops remain incomplete: buffered tools are withheld and nothing is automatically
  retried.
- Text system/user/assistant messages, function calls/results, and unsigned
  reasoning history. The client must resend the conversation history.
- An optional `user` field provides session identity; use a distinct value for
  each independent conversation. Without it, each request gets a fresh identity.
- Live requests mark the system prompt (when present) and last non-system message
  as EPHEMERAL cache breakpoints, using that same session identity as the upstream
  cache key. Cache reuse is upstream best-effort, not guaranteed; there is no
  background warming or extra cache configuration.
- With `stream_options: {"include_usage": true}`, successful live streams report
  upstream usage before the final completion marker and `[DONE]`. `prompt_tokens`
  includes uncached input, cache writes and cache reads; `completion_tokens` is
  output and `total_tokens` is their sum. Reported cache reads appear in
  `prompt_tokens_details.cached_tokens`, and writes in `cache_write_tokens` (an
  extension in the same details object). Missing cache counters are omitted, not
  inferred from latency; cumulative snapshots are merged rather than added.
  Failed/incomplete streams do not emit success usage.
- The client executes tools and must implement its own approval policy. This
  helper does not execute tools or supply an agent sandbox.
- Unsupported fields are rejected. No Responses API, embeddings, images,
  non-streaming mode, signed or redacted reasoning support.

The discovery field mapping is checked against the public
[`devin-proto.ts`](https://github.com/can1357/oh-my-pi/blob/main/packages/catalog/src/discovery/devin-proto.ts)
and [`devin.ts`](https://github.com/can1357/oh-my-pi/blob/main/packages/catalog/src/discovery/devin.ts)
schemas. In particular, config field 4 is `disabled`, not image support. The helper
does not collapse model families or substitute aliases; encoded requests and all
SSE chunks (including usage) preserve the selected UID.

Tools and the final completion marker are withheld until the upstream response
passes terminal validation. Partial text may already have streamed when an error
occurs. Clients must treat interrupted exchanges as incomplete and must not
blindly retry tool effects. Disconnecting cannot undo a tool already executed by
another application.

## Security and data flow

The server binds only to IPv4 loopback, authenticates requests with the local key,
and admits at most five concurrent requests. Live upstream URLs are fixed vendor
HTTPS endpoints. Proxy configuration, redirects and automatic retries are disabled.
Provider credentials, prompts, conversation history and tool results are sent to
the upstream service as required by its protocol. Local request authentication is
not a substitute for isolating untrusted local processes. Do not expose the port
through a tunnel or public reverse proxy.

`--offline-upstream ADDRESS` is a synthetic test mode restricted to literal
loopback HTTP addresses. Its request contract is deliberately narrower than live
mode; it is not an alternative provider configuration.

## Releases

Update the Cargo package version and lockfile, then push a matching `v<version>`
tag after reviewing the changes. GitHub Actions tests each native target, builds
release binaries with remapped build paths, packages the executable and license,
verifies checksums and publishes a release only after every target succeeds.
Tagging and publishing are maintainer actions, not performed by local builds.

Version 0.2.0 adds dynamic model discovery through `/v1/models`; version 0.1.0
cannot serve that endpoint. Existing binaries do not update themselves. To update,
download the archive for your platform and `SHA256SUMS` from the same release,
verify the archive's SHA-256 checksum, then extract it into a temporary directory.
Stop clients and bridge processes before replacing the executable at the path you
configured, preserving its executable permission on Unix. Restart clients with
fresh conversations and refresh their model catalog. Do not overwrite a different
executable found on PATH or a client-managed installation without checking which
path the client actually uses.

## License

MIT. See [LICENSE](LICENSE).