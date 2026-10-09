# SDKs AGENTS

You are working on OpenSandbox SDKs. Keep generated and handwritten code separate, and keep behavior aligned across languages when the same capability exists in multiple SDKs.

## Scope

- `sandbox/**`
- `code-interpreter/**`
- `mcp/**`
- workspace-level SDK build and release metadata

If the task is driven by spec changes, also read `../specs/AGENTS.md`.

## Key Areas

- `sandbox/python`, `sandbox/javascript`, `sandbox/kotlin`, `sandbox/csharp`, `sandbox/go`, `sandbox/rust`
- `code-interpreter/python`, `code-interpreter/javascript`, `code-interpreter/csharp`, `code-interpreter/rust`
- `sandbox/kotlin/code-interpreter`
- `mcp/sandbox/python`
- Workspace config in `package.json`, `pnpm-workspace.yaml`, and shared build files

## Generated Code

Do not manually edit generated code as the only fix.

Generator-owned paths include:

- `sandbox/python/src/opensandbox/api/**`
- `sandbox/javascript/src/api/*.ts`
- `sandbox/rust/src/api/**` (owned by `sandbox/rust/scripts/generate_api.py`)
- `sandbox/kotlin/sandbox-api/build/generated/**`
- language-specific OpenAPI outputs produced by local generator scripts or Gradle tasks

Handwritten logic belongs in adapters, services, facades, converters, and stable SDK models.

## Commands

Workspace JS install:

```bash
cd sdks
pnpm install --frozen-lockfile
```

JavaScript SDK checks:

```bash
cd sdks
pnpm run lint:js
pnpm run typecheck:js
pnpm run build:js
pnpm run test:js
```

Python sandbox SDK:

```bash
cd sdks/sandbox/python
uv sync
uv run python scripts/generate_api.py
uv run ruff check
uv run pyright
uv run pytest tests/ -v
uv build
```

Python code-interpreter SDK:

```bash
cd sdks/code-interpreter/python
uv sync
uv run ruff check
uv run pyright
uv run pytest
uv build
```

JavaScript sandbox SDK:

```bash
cd sdks/sandbox/javascript
pnpm run gen:api
pnpm run lint
pnpm run typecheck
pnpm run build
pnpm run test
```

JavaScript code-interpreter SDK:

```bash
cd sdks/code-interpreter/javascript
pnpm run lint
pnpm run typecheck
pnpm run build
pnpm run test
```

Kotlin sandbox SDK:

```bash
cd sdks/sandbox/kotlin
./gradlew :sandbox-api:generateLifecycleApi :sandbox-api:generateExecdApi :sandbox-api:generateEgressApi
./gradlew spotlessApply :sandbox:test :code-interpreter:test
```

Go sandbox SDK:

```bash
cd sdks/sandbox/go
go test ./...
```

Rust sandbox SDK (independent Rust 2021 crate, MSRV 1.85):

```bash
cd sdks/sandbox/rust
# Match CI's generator environment and formatter for reproducible output.
python3 -m pip install PyYAML==6.0.2
rustup toolchain install 1.85.1 --profile minimal --component rustfmt --component clippy
python3 -m unittest discover -s scripts -p 'test_*.py'
RUSTUP_TOOLCHAIN=1.85.1 python3 scripts/generate_api.py
RUSTUP_TOOLCHAIN=1.85.1 python3 scripts/generate_api.py --check
cargo +1.85.1 fmt --all -- --check
cargo +1.85.1 clippy --locked --all-targets --all-features -- -D warnings
cargo +1.85.1 test --locked --all-features
RUSTDOCFLAGS='-D warnings' cargo +1.85.1 doc --locked --no-deps --all-features
cargo +1.85.1 package --locked
```

CI uses Python 3.11 and PyYAML 6.0.2 for generation, and tests both MSRV and
stable Rust. The generator owns only `sandbox/rust/src/api/**`, not Cargo
manifests, lockfiles, handwritten services, or other language SDK outputs.
Keep both Rust crates' `Cargo.lock` files checked in and use `--locked` for checks.
Validate the default, `redis`, `tracing`, and combined sandbox feature sets.
For the independent interpreter, run the same fmt/clippy/test/doc checks from
`code-interpreter/rust`, plus `cargo build --locked --examples` and
`cargo package --locked --list`. The interpreter's full registry package check
is blocked until its core `opensandbox` dependency is published. Local path
builds and a package file-list check do not constitute registry package verification.
Redis integration tests require an explicitly dedicated Redis service:
`OPENSANDBOX_REDIS_TEST=1 OPENSANDBOX_REDIS_URL=redis://127.0.0.1:6379/0 cargo test --locked --features redis --test redis_pool`.
Real sandbox workflows are documented in `../tests/rust/README.md`.
Neither crate is published on crates.io. Do not add registry installation
instructions or release automation until publication is configured.

## Guardrails

Always:

- For spec-driven changes, regenerate affected SDK code, update handwritten layers, then run affected language checks.
- For MCP changes, keep tool schemas, client setup docs, and sandbox SDK dependency behavior aligned.
- Add a regression test for every bug fix.
- Prefer tests for request mapping, response conversion, error mapping, streaming behavior, and resource cleanup.
- Keep package-local validation fast before widening to multi-language verification.
- Match public behavior across languages unless a documented platform constraint prevents it.
- Keep wire-format units and public SDK units separate. Public SDK interfaces should expose time durations as language-native duration types where available (`timedelta`, `Duration`) or otherwise as explicitly second-based fields such as `timeoutSeconds`.
- For Kotlin SDK public APIs intended for Java interoperability, do not expose Kotlin value classes such as `kotlin.time.Duration`; they are JVM-name-mangled and can be inaccessible from Java. Prefer `java.time.Duration` or explicit primitive wire units at the public boundary, with deprecated Kotlin-friendly overloads when needed for migration.

Ask first:

- Public breaking changes
- Large cross-language refactors
- Intentional behavior drift between languages

Never:

- Patch generated output as the only fix.
- Change SDK public behavior without tests.
- Mix unrelated non-SDK work into an SDK change.

## Good Patterns

- Generated clients for normal request/response APIs
- Handwritten transport only for streaming or protocol-specific paths such as SSE
