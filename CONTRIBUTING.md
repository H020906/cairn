# Contributing to Cairn

This project is built to be picked up by people who did not write it. If something here is
unclear, that is a bug in this document — please say so.

## Before anything else

Read **[ARCHITECTURE.md](ARCHITECTURE.md)**. Cairn looks like an ordinary distributed job
system and is not one; the verification protocol is the whole point, and a change that
seems harmless can quietly break it. Then read
**[ADR-0001](docs/adr/0001-verification-by-dispute-not-replication.md)**.

## Local setup

You need:

| Tool | Version | Notes |
|---|---|---|
| JDK | 21+ | Coordinator. Maven comes from the wrapper — do not install it |
| Rust | stable | `rustup target add wasm32-unknown-unknown` |
| Node | 20+ | Dashboard |
| Docker | any recent | PostgreSQL + Redis |

```bash
git clone <repo> && cd cairn
docker compose up -d          # PostgreSQL + Redis
cd server && ./mvnw spring-boot:run
cd web && npm install && npm run dev
```

On Windows without the MSVC C++ workload, use the GNU host toolchain — it links without
Visual Studio:

```bash
rustup default stable-x86_64-pc-windows-gnu
```

## The one rule that is not negotiable

**Nothing may introduce non-determinism into workload execution.**

Cairn settles disputes by finding the first instruction where two workers diverged. If two
*honest* workers can produce different traces, the protocol convicts an innocent volunteer.
That failure is silent, rare, and concentrated on unusual hardware — the worst possible
combination.

Concretely, in anything under `runtime/`:

- No wall-clock time, no system entropy, no I/O, no thread scheduling, no address-dependent
  behaviour.
- No `HashMap` iteration order in anything that reaches a trace.
- Floating point must follow the instrumented module's semantics exactly — do not "optimise"
  a NaN canonicalization away because it looks redundant.
- If you touch the instrumentation pass or either engine, the differential fuzzer must pass.
  It runs in CI and it is not advisory.

## Tests

| Scope | Command |
|---|---|
| Rust | `cargo test --workspace` |
| Rust lint | `cargo clippy --workspace --all-targets -- -D warnings` |
| Java | `cd server && ./mvnw verify` |
| Web | `cd web && npm test` |

Integration tests use Testcontainers and need Docker running. They are not optional — the
assignment and lease logic is concurrent and cannot be trusted to unit tests alone.

## Pull requests

- One concern per PR. A refactor and a behaviour change in the same diff will be sent back.
- New behaviour comes with a test that fails without the change.
- Public Rust items and Java classes get doc comments explaining *why*, not *what*.
- If your change invalidates something in `docs/adr/`, supersede the ADR in the same PR.

## Good first issues

Look for the `good-first-issue` label. If there are none open, these are always welcome:

- Additional scientific workloads under `workloads/`
- Determinism test cases — especially nasty floating-point corner cases
- Accessibility and i18n on the dashboard
- Documentation that explains something you had to work out yourself

## Scope of ambition

Cairn is deliberately narrow: make volunteer computing cheap to join and cheap to verify.
Proposals that broaden it (payments, tokens, a general compute marketplace, GPU workloads)
will be read seriously but start from a position of scepticism — see the *deliberately not
here* section of [ARCHITECTURE.md](ARCHITECTURE.md).

## Code of conduct

Be decent. Assume the person you are replying to is smart, busy, and acting in good faith.
Disagree with the argument.
