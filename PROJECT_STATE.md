# Project State

**This file is the project's memory.** It is the first thing to read when
picking the work up, and the last thing to update before finishing a session.
`PROJECT_SPECS.md` holds the design and changes rarely; this file tracks where
the work actually is and is expected to change constantly.

**Last updated:** 2026-08-31
**Build order position:** steps 1-5 complete; step 6 is next

Deliberately no commit hash here — git already knows it, and a hash copied into
prose is stale the moment the next commit lands.

## Where we are

**Debates run.** `coin debate` puts two models against each other in the
credence format, streams the transcript, and reports convergence. Verified on
real questions: both sides used web search, checked primary sources, and a side
whose assigned position the evidence contradicted conceded honestly rather than
defending it. That is the behaviour the whole project is for.

There is no web UI yet, and only one of the four formats exists. `probe`
remains as a single-prompt diagnostic.

## Done

### Step 1-2: specification and scaffold

- `PROJECT_SPECS.md` — 14 sections, the agreed design. Read this first.
- `cargo init`, dependencies, `error.rs` (`CoinError` via `thiserror`),
  `config.rs` (timeouts, data directory resolution), tracing to stderr.

### Step 3-4: opencode integration

| Module | What it does |
|---|---|
| `src/opencode/process.rs` | Spawns `opencode serve --port 0`, parses the port from stdout, polls `/api/health`, kills the child on drop |
| `src/opencode/client.rs` | `OpencodeClient` trait plus `HttpClient`: sessions, prompt, abort, model listing |
| `src/opencode/events.rs` | Consumes the `/event` SSE bus, decodes to `ServerEvent`, hands to a handler returning `Flow::Continue`/`Stop` |
| `src/opencode/types.rs` | Wire types: `ModelRef`, `Part`, `AssistantMessage`, `ServerEvent`, `ProvidersResponse` |
| `src/opencode/workspace.rs` | Prepares a directory as an opencode project, including the required `git init` |
| `src/main.rs` | `coin probe` |

`OpencodeClient` is a trait from the outset so the engine can be tested against
a mock with no network and no model spend. Nothing outside `src/opencode/` knows
opencode's wire format.

### Step 5: the debate engine

| Module | What it does |
|---|---|
| `src/debate/state.rs` | `Side`, `Topic`, `Credence`, `Claim`, `TurnAnalysis`, `Turn`, `StopReason`, `DebateState` |
| `src/debate/format.rs` | `DebateFormat` trait, `FormatId`, and the shared truth-seeking mandate |
| `src/debate/parse.rs` | Tolerant extraction of the trailing fenced json block |
| `src/debate/credence.rs` | The credence-updating format |
| `src/debate/engine.rs` | Orchestrator: two sessions, alternating turns, stop conditions |
| `src/store.rs` | Transcript persistence: versioned JSON plus readable Markdown |
| `src/term.rs` | Terminal colour, honouring NO_COLOR and non-TTY output |
| `src/main.rs` | `coin debate`, with live per-side colour streaming |

Personas are delivered as generated agent files, which replace opencode's
built-in coding prompt rather than adding to it. Each debate gets a disposable
git-initialized workspace.

Arguments stream token by token in a colour per side, with the model named in
the header. The default debater model is `digitalocean/glm-5.3-flash`, picked by
benchmarking on real debates: about a fifteenth the cost of the previous default
while still well calibrated.

Every debate auto-saves `transcript.json` and `transcript.md` into its
workspace; `--save FILE` writes an extra copy anywhere.

**Tests:** 112 library tests, 8 binary tests, and 6 doctests run offline and
free. 4 integration
tests marked `#[ignore]` drive a real server, take about 5 seconds, and cost
well under a cent.

## Not done

Everything below is unbuilt.

| Step | Work | Notes |
|---|---|---|
| 6 | axum server, the section 9 API, snapshot-first SSE, minimal UI | **Start here** |
| 7 | Remaining three formats: crux-finding, classic rounds, claim ledger | |
| 8 | Intervention commands: pause, resume, step, inject, reroll, edit, abort | |
| 9 | Permission and question cards in the UI | Needed because debaters have full tool access |
| 10 | Judge pass; transcript persistence is already done | |
| 11 | Analysis rail: convergence chart, claim ledger, crux tree | Chart geometry computed in Rust, JS only emits SVG |
| 12 | Optional, post-v1: `/api/openapi.json` | Deferred until types settle |

## Next session: start here

Step 6, the web layer. Concretely:

1. `src/web/routes.rs` — the axum router and the section 9 API surface.
2. `src/web/stream.rs` — `tokio::sync::broadcast` to SSE, emitting a `Snapshot`
   event on connect before any live event.
3. `src/web/api.rs` — start a debate, read state, export.
4. `web/index.html`, `app.js`, `styles.css` — Pico CSS and vanilla JS only.
5. Refit `Engine::run` to publish `Progress` into the broadcast channel rather
   than a closure, and to stream token deltas from the opencode event stream.

**Resolve first:** agent files are read at server startup, but the web server
outlives many debates, so personas cannot be written per debate the way the CLI
does it. Either restart opencode per debate, or move personas into each
session's first message and accept the built-in prompt remaining in place. See
`PROJECT_SPECS.md` section 5.5.

## Upstream facts that cost time to find

Full detail in `PROJECT_SPECS.md` section 5. Summarized here so they are not
rediscovered:

1. **The project directory must be a git repository** or opencode reports an
   empty model catalog. Prompting still works via the default model, so the
   symptom is a blank model picker, not an error. Handled by
   `workspace::prepare`.
2. **`message.part.delta` carries its fragment in `delta`, not `text`**, with a
   `field` discriminator separating visible text from `reasoning` tokens.
3. **`model` in the prompt payload must be an object**,
   `{providerID, modelID}`. A bare string is rejected. Model ids can contain
   slashes, so `provider/model` splits on the first separator only.
4. **Model listing is `GET /config/providers`**, not `GET /api/model`. The
   latter returns only opencode's own hosted catalog and omits every configured
   provider.
5. **serde's `#[serde(other)]` cannot express "ignore unknown events"** for an
   adjacently tagged enum. `ServerEvent` decodes by hand so unmodelled events
   degrade instead of killing the stream.
6. A health poll is mandatory before the first request; opencode briefly serves
   HTML from API routes right after binding.
7. **A turn is not one message.** `POST /session/{id}/message` returns only the
   last assistant message; tool calls, reasoning, and part of the cost live in
   earlier ones. The engine re-reads `GET /session/{id}/message` and aggregates
   everything after the last user message.
8. **Agent files replace the built-in system prompt** and are read at server
   startup, so they must be written before launch.

## Open questions

- **Personas versus a long-lived server.** Agent files are read at opencode
  startup. The CLI writes both personas then launches, but a web server outlives
  many debates. Blocks step 6; see "Next session".
- **Debate cost.** Now roughly $0.008 for a two-turn debate on the default
  model, down from about $0.24 per four turns on kimi-k3. Still worth surfacing
  running cost in the UI.
- **Speed varies enormously by model.** The default takes around 90 seconds per
  turn with web search, and a two-round debate can exceed 400 seconds. One
  candidate needed over 400 seconds for two rounds. Slow turns will matter more
  once a browser is waiting on them.
- **The default model choice rests on one benchmark run.** gpt-oss-120b was
  rejected for reporting 20 percent confidence in an obviously supported
  position, but in a later run it produced a perfectly calibrated result at an
  eighth the cost and three times the speed. That first result may have been
  noise. Worth several repeated runs per candidate before trusting the default.
- **Reasoning still leaks into visible text.** Some models stream their internal
  monologue as ordinary argument. Turn cards will need to handle it.
- **Structured blocks fail more often than expected in long debates.** A real
  six-round run produced only two readable credences per side, meaning eight of
  twelve turns had no usable structure. The convergence check runs on the most
  recent readable value, so it can compare stale numbers across a long gap. The
  count is now reported, but the underlying parse rate is worth investigating.
- **Cheap models may fabricate tool use.** Several answered as though they had
  run a tool without calling one. Model choice for debaters is a correctness
  concern, not only a cost one.
- **A listed model is not necessarily usable.** DigitalOcean rejects parts of
  its catalog with subscription-tier and not-found errors, surfacing as an
  opaque `UnknownError` with the real cause only in the server log.

## Running it

```bash
cargo run -- debate \
  -q "the question under dispute" \
  -a "the case for side A" \
  -b "the case for side B" \
  -r 2                        # max rounds; -m to pick a model,
                              # --model-b to differ the two sides
                              # --save FILE for an extra transcript copy

cargo run -- probe "your question"                           # one prompt, streamed
cargo run -- probe -m digitalocean/openai-gpt-oss-20b "..."  # pick a model
RUST_LOG=coin=debug cargo run -- probe "..."                 # verbose
```

`-q`, `-a` and `-b` accept either literal text or a path to a file holding it,
which is easier for positions that run to paragraphs. A value that looks like a
path but does not exist is rejected rather than debated literally.

Only the `credence` format is implemented; the other three are step 7 and are
rejected with a clear message.

Arguments stream live in colour, cyan for side A and magenta for side B.
Styling switches off automatically when output is redirected; `NO_COLOR`
disables it and `CLICOLOR_FORCE=1` forces it on through a pipe.

stdout carries the model's text, stderr everything else, so it pipes cleanly.

```bash
cargo test                                                   # offline, free
cargo test --test opencode_integration -- --ignored          # ~5s, <1 cent
COIN_TEST_MODEL=digitalocean/kimi-k3 cargo test --test opencode_integration -- --ignored
```

Tests pin `digitalocean/openai-gpt-oss-20b` (about $0.0004 and 1s per call)
rather than the server default (about $0.0136 and several seconds).

Before committing: `cargo test`, `cargo clippy --all-targets -- -D warnings`,
`cargo fmt --check`. All currently pass.

## Conventions in force

- `AGENT_rust.md` is binding: no `.unwrap()` in production paths, `where`
  clauses for generic bounds, `AsRef` on public functions taking strings or
  paths, doc comments on every public item, no emoji or emoji-like unicode
  anywhere including the UI, 4-space indent, 100-column lines.
- The web UI, when built, is **Pico CSS and vanilla JavaScript only**. No React,
  no jQuery, no component framework. Adaptive light and dark with a toggle.
- "Commit" means commit **and** push.
- **Every design decision is mirrored into `PROJECT_SPECS.md`** as part of the
  same change that acts on it, whether the user stated it, we reached it
  together, or an upstream discovery forced it. A decision that lives only in a
  conversation is lost when the session ends.

## Updating this file

Update it **in the same commit as the work it describes**, not as a separate
tidying pass later. A memory file maintained retroactively is a memory file that
is usually wrong.

What to touch, and when:

| Trigger | Update |
|---|---|
| A build-order step finishes | Move it from "Not done" to "Done"; rewrite "Next session: start here" for the new next step |
| A design decision is made | `PROJECT_SPECS.md` first, then note here only if it changes what is left to build |
| An upstream dependency surprises us | Add to "Upstream facts", with the detail in `PROJECT_SPECS.md` section 5 |
| A question is raised that is not answered now | Add to "Open questions", saying what it blocks |
| A question gets answered | Delete it from "Open questions" — resolved items are noise |
| A command, flag, or test workflow changes | Update "Running it" |
| Any of the above | Bump "Last updated" |

Two habits that keep it honest: prefer deleting stale content over accumulating
it, and keep "Next session: start here" specific enough to act on without
rereading the code. If a section is no longer true, it is worse than absent.
