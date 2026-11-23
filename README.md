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
read credential files. The protocol currently identifies as `devin-desktop`.
The example values above are placeholders, not working credentials.

Read the first stdout line for the allocated `127.0.0.1:PORT` address. Configure
your client with base URL `http://127.0.0.1:PORT/v1`, the local bearer key, model
`swe-2-flash-medium`, and streaming enabled. Keep the process running for the
client's session; terminate it when finished. There is no fixed port or automatic
restart. Never put provider credentials in command arguments, shell history,
checked-in files, or request logs.

## Supported API

- `POST /v1/chat/completions`, bearer-authenticated, streaming SSE only.
- Only `swe-2-flash-medium`; output limited to 1–4,096 tokens.
- Text system/user/assistant messages, function calls/results, and unsigned
  reasoning history. The client must resend the conversation history.
- An optional `user` field provides session identity; use a distinct value for
  each independent conversation. Without it, each request gets a fresh identity.
- The client executes tools and must implement its own approval policy. This
  helper does not execute tools or supply an agent sandbox.
- Unsupported fields are rejected. No Responses API, model-list endpoint,
  embeddings, images, non-streaming mode, signed or redacted reasoning support.

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
release binaries with remapped build paths, packages only the executable, verifies
checksums and publishes a release only after every target succeeds. Tagging and
publishing are maintainer actions, not performed by local builds.

## License

MIT. See [LICENSE](LICENSE).