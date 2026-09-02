# Coin — Project Specification

Version 0.1.0 (draft)

> This document is the design: what coin should be, and why. **Every design
> decision belongs here**, including ones reached mid-conversation and ones
> forced by something discovered about an upstream dependency — recorded as
> part of the same change that acts on the decision, not afterwards.
>
> `PROJECT_STATE.md` is the companion: what is built so far, what is left, and
> where to pick up. It tracks progress and links back to the sections here
> rather than restating them.

## 1. Purpose

Coin runs a structured debate between two LLM debaters over a proposition the
user supplies, and streams it live to a local web UI so the reasoning can be
watched, steered, and audited as it happens.

The design goal is **truth-seeking, not persuasion**. Both debaters are
instructed that arriving at the ground truth outranks winning, that conceding a
point is a success rather than a failure, and that stating a confidence honestly
matters more than defending an assigned position. The debate formats are chosen
to make either convergence, or the precise point of irreducible disagreement,
visible and inspectable.

Debaters have the **full capability of an opencode agent**: all tools, skills,
subagents, and web search. A debater arguing about a benchmark result can run
the benchmark. A debater arguing about an API's behavior can read its source.
This is the central bet of the project — a debate where claims can be checked
beats a debate where they can only be asserted.

### 1.1 Non-goals

- Not a general chat interface. One debate at a time, one purpose.
- Not a scoring or leaderboard system for models.
- Not multi-user. Binds to loopback, single operator.
- Not a persuasion or rhetoric trainer.

## 2. User-facing behavior

The user supplies a question and both sides of the argument, picks a format and
models, and launches. The UI then shows two columns of live-streaming argument,
with tool invocations and citations inline, plus a format-dependent analysis
rail. The user can pause, inject guidance to either side, re-roll a weak turn,
edit a turn, or abort at any point.

### 2.1 Inputs

| Field               | Type    | Notes                                                      |
| ------------------- | ------- | ---------------------------------------------------------- |
| Question            | text    | The proposition under dispute                              |
| Position A          | text    | The case the first debater is assigned                     |
| Position B          | text    | The case the second debater is assigned                    |
| Format              | enum    | One of the four in section 4                               |
| Model A / B / Judge | string  | `provider/model`, defaults to the same model on both sides |
| Max rounds          | integer | Hard cap, default 3                                        |
| Tools enabled       | multi   | Defaults from detected tool availability                   |
| Judge summary       | bool    | Default on                                                 |
| Step mode           | bool    | Default off; pause after every turn                        |
| Auto-approve        | bool    | Default off; see section 7                                 |

Assigning the same model to both sides is the default because it isolates the
argument from model capability. Differing models are supported and are the more
interesting configuration once the basics work.

**The default debater model is `openrouter/z-ai/glm-5.3-flash`**, chosen by
benchmarking candidates on real debates rather than on price alone. It costs
roughly a fifteenth of the server default while still stating well-calibrated
confidences. That last property is not optional: the cheaper models tried were
faster and cheaper still, but reported low confidence in positions the evidence
plainly supported, which corrupts the convergence reading the whole format
rests on. One candidate produced no output at all despite being listed as
available.

Model choice is therefore a correctness decision, not only a cost one, and the
model in use is displayed in the debate header so a suspicious result can be
attributed.

**The route changed, not the model.** The same weights were originally used as
`digitalocean/glm-5.3-flash`, which the benchmark above was run against.
DigitalOcean charges $0.150/$0.250 per million input/output tokens against
OpenRouter's $0.075/$0.250, and it enforces a daily token quota that ended a
real debate mid-run: every turn after the limit came back empty. Moving the
route keeps the calibration result, halves the price, and removes that wall. It
is not a new model choice and does not need re-benchmarking.

### 2.1b Defaults come from a configuration file

Choosing a model should not be a recompile, and typing `-m
openrouter/z-ai/glm-5.3-flash` on every invocation is worse. Coin therefore
reads a `config.toml`:

```toml
[debate]
model = "openrouter/z-ai/glm-5.3-flash"
model_b = "openrouter/openai/gpt-oss-120b"   # optional
```

Three sources settle each value, in falling order of precedence: **the command
line, then the file, then the built-in default.** `-m` is documented as setting
both sides, so it overrides a `model_b` the file pinned; `--model-b` is how to
differ the sides for a single run.

The file is read from the working directory, or from the path given to
`--config`. The two are treated differently on purpose: an absent `config.toml`
is the ordinary case and means "use the defaults", while a file named
explicitly must exist, because silently ignoring `--config typo.toml` would run
a debate with settings the operator believes they replaced.

Unknown keys are **rejected** rather than ignored, for the same reason: a
misspelled `moddel` that quietly does nothing is worse than a startup error
naming the line. Models are validated at load, so `provider/model` form is
enforced against the file and column, not discovered at the first prompt.

Only debate defaults live there today. `probe` keeps taking the server's own
choice unless `-m` says otherwise, since its purpose is exercising the
integration path rather than producing a result.

`samples/config.toml` is the commented example, and a test loads it so a
sample that stops parsing fails the build. A `config.toml` in the working
directory is git-ignored, being local preference rather than project design.

### 2.1a Topic arguments accept a file or a string

The question and both positions accept either literal text or a path to a file
containing it. A position argued well is often several paragraphs, which is
awkward to paste into a shell and easy to lose. An existing file is read and
trimmed; any other value is used verbatim.

The obvious hazard of that rule is a mistyped filename silently becoming the
argument, so a debate runs about the literal string `notes/postion.md`. A value
that does not exist is therefore rejected when it **looks** like a path:
no whitespace, and either a separator, a leading `~`, or a `.txt`, `.md`, or
`.text` extension. Whitespace is checked first so a genuine question containing
a slash, such as "Is TCP/IP better than the OSI model?", stays a literal
string. An empty file is an error rather than an empty position.

### 2.2 Transcripts

**Every debate is saved without being asked for.** A debate costs real money
and several minutes, and the interesting part is often a single concession
buried mid-argument, so losing one to a closed terminal is a real loss.

Two files are written into the debate's workspace, because they serve
different readers:

- `transcript.json` carries the complete state, including every credence,
  tool call, token count, and parse status. This is what
  `GET /api/transcript.json` will serve.
- `transcript.md` is for people: the topic, each turn with its tool use and
  concessions, and the closing convergence table.

The JSON is versioned so a later reader can recognise an older file, and
records the format and models, which `DebateState` alone does not. Which model
produced a result is exactly what a reader needs months later.

`--save <FILE>` writes an additional copy anywhere; a `.json` extension selects
JSON, anything else selects Markdown, and missing parent directories are
created.

### 2.3 Command line rendering

A debate streams two voices into one terminal, so the sides are coloured
distinctly and each side's argument is written token by token as the model
produces it, rather than appearing complete once the turn ends. Watching a
debater change its mind mid-paragraph is most of the value of running one
interactively.

The style is opened once per turn and closed at its end, so streaming emits two
escape sequences per turn rather than a pair per token. Tool invocations
interrupt the stream on a dimmed line and then restore the side's colour.

Styling is suppressed when standard output is not a terminal, so piping a
debate to a file yields clean text. `NO_COLOR` disables it, `CLICOLOR_FORCE=1`
forces it on; when both are set, suppression wins.

## 3. Architecture

```
                  browser (Pico CSS + vanilla JS)
                        |  SSE /api/stream      ^ POST /api/control
                        v                       |
        +---------------------------------------------------+
        |  coin (Rust, axum + tokio)                         |
        |                                                    |
        |  web::stream  <-- broadcast::Sender<DebateEvent>   |
        |        ^                                           |
        |  debate::engine  (orchestrator task, state machine)|
        |        |  DebateFormat trait -> 4 impls            |
        |        v                                           |
        |  opencode::client (reqwest + SSE consumer)         |
        +---------------------------------------------------+
                        |  REST + /event SSE
                        v
              opencode serve  (child process, random port)
                        |
              DigitalOcean / OpenRouter  + Exa websearch
```

There are two SSE hops. The inner hop consumes opencode's raw event firehose.
The engine translates it into a small domain event enum. The outer hop
broadcasts that enum to every connected browser. The browser never sees
opencode's wire format, so a schema change upstream is absorbed in one module
rather than rippling into the UI.

### 3.1 Why opencode as the transport

The program never handles an API key. Credentials for DigitalOcean and
OpenRouter already live in `~/.local/share/opencode/auth.json`, and driving
`opencode serve` inherits them along with model routing, tool execution, skill
loading, and subagent spawning. Reimplementing that surface against raw
provider APIs would be a large amount of work to arrive at less capability.

The cost is a child process to supervise and session-shaped rather than
chat-shaped semantics. Section 5 covers both.

## 4. Debate formats

Formats are selected per debate, because the right structure depends on the
question. An empirical dispute with a checkable answer wants credence tracking;
a definitional or values dispute wants classic rounds. All four implement one
trait, so a fifth is purely additive.

```rust
pub trait DebateFormat: Send + Sync {
    /// Stable identifier used in configuration and persistence.
    fn id(&self) -> FormatId;

    /// System prompt establishing the persona and the truth-seeking mandate.
    fn system_prompt(&self, side: Side, topic: &Topic) -> String;

    /// Prompt for this side's next turn, given everything that has happened.
    fn turn_prompt(&self, state: &DebateState, side: Side) -> String;

    /// Extract format-specific structure from a completed turn.
    fn parse_turn(&self, raw: &str) -> TurnAnalysis;

    /// Whether the debate has reached its natural end.
    fn should_stop(&self, state: &DebateState) -> Option<StopReason>;
}
```

### 4.1 Crux-finding

Rounds narrow toward the single load-bearing disagreement.

1. State position and the one claim it most depends on.
2. Identify the opponent's load-bearing claim.
3. State explicitly what evidence would change your mind.
4. Narrow to the shared crux.

Stops when both sides name the same crux. Output is the crux plus what would
settle it. Best fit when the disagreement is real but the parties have not
located it.

### 4.2 Credence updating

Every turn restates a 0-100 confidence and justifies any movement since the
previous turn. Refusing to move without reason is called out in the prompt as a
failure mode.

**Convergence is `abs(a + b - 100) <= 15`, not `abs(a - b)`.** Each side reports
confidence in **its own** assigned position, so the two numbers describe
different propositions. When the sides agree, one is near 0 and the other near 100. Comparing them directly reports maximum disagreement at the exact moment
the debate has succeeded, and reports agreement when both sides are certain of
opposing positions. Restating B's confidence on A's proposition is `100 - b`,
which makes the gap `abs(a - (100 - b))`.

This was found by running a real debate: side A verified its assigned position,
found the evidence contradicted it, and dropped to 3 while B rose to 99. Total
agreement, reported as a 96 point gap and "no convergence".

Convergence is additionally suppressed until **two complete rounds** have
happened. Two models under a truth-seeking mandate sometimes agree immediately,
producing a one-round debate containing no exchange of evidence. An outright
concession, a credence of 0 in one's own position, still ends the debate at any
point, because that is a genuine resolution rather than premature agreement.

Produces the convergence chart in the analysis rail, which is the most legible
signal in the whole product.

**A credence reading carries the round it came from.** Turns whose structured
block is unreadable state no credence, so the Nth reading is not the Nth round:
a six-round debate can yield two readings. Labelling readings by their position
in the series misreports which round produced them, which is misleading in
exactly the place a reader looks to judge the result.

For the same reason, **the count of turns with no readable structure is
surfaced** alongside the table. Parsing degrades rather than failing, by design,
but a debate that completes with most of its structure unreadable silently
weakens every number drawn from it. A sparse table must not pass for a short
debate.

### 4.3 Classic rounds

Opening, rebuttal, cross-examination, closing, then a judge verdict. Familiar
and readable. Appropriate when convergence is not expected and the value is in
seeing both cases stated as strongly as possible.

Stops after the closing statements.

### 4.4 Claim ledger

Every turn is parsed into discrete claims tagged `agreed`, `disputed`, or
`unresolved`, accumulating into a shared ledger. Truth is read off the ledger
rather than off the rhetoric.

Stops when a full round introduces no new claims.

### 4.5 Common rules

All formats respect the hard round cap. Unless disabled, all end with a judge
pass over the full transcript producing a verdict and a confidence.

## 5. opencode integration

Facts in this section were verified against opencode 1.18.20 by probing a live
server and inspecting the shipped binary. They are version-sensitive.

### 5.1 Process supervision

`opencode::process` spawns:

```
opencode serve --port 0 --hostname 127.0.0.1
```

with `OPENCODE_ENABLE_EXA=1` and a freshly generated `OPENCODE_SERVER_PASSWORD`
in the environment. It parses the `opencode server listening on
http://127.0.0.1:PORT` line from stdout to discover the port.

**A health poll is mandatory before the first request.** Immediately after
launch the server briefly serves the web UI's HTML from `POST /api/session`
instead of JSON. Poll `GET /api/health` until `{"healthy":true}`.

**The project directory must be a git repository.** opencode resolves its model
catalog per project, and a directory outside a repository yields an empty one:
`GET /api/model` returned zero entries against a plain directory and 30 against
the same directory after `git init`. Prompting still succeeds using the
server's default model, so the failure is quiet — the symptom is an empty model
picker, not an error. `opencode::workspace::prepare` initializes a repository
before launch for this reason, which also means every per-debate workspace is a
repository and a debate's file changes are diffable.

The child is killed on drop and on SIGINT. Orphaned servers are a bug.

### 5.2 REST surface used

opencode exposes two coexisting API surfaces: legacy flat routes and a v2
surface under `/api/*` that wraps responses in `{"data": ...}`. We use the
legacy routes for session work, where the response shape is flatter, and v2 for
health and model listing.

| Purpose                 | Call                                                    |
| ----------------------- | ------------------------------------------------------- |
| Health                  | `GET /api/health`                                       |
| Model list              | `GET /config/providers`                                 |
| Tool availability       | `GET /experimental/tool`                                |
| Create session          | `POST /session`                                         |
| Send turn               | `POST /session/{id}/message`                            |
| Abort turn              | `POST /session/{id}/abort`                              |
| Delete message (reroll) | `DELETE /session/{id}/message/{messageID}`              |
| Patch part (edit)       | `PATCH /session/{id}/message/{messageID}/part/{partID}` |
| Answer permission       | `POST /session/{id}/permissions/{permissionID}`         |
| Answer question         | `POST /session/{id}/question/{requestID}/reply`         |
| Event stream            | `GET /event`                                            |

The prompt payload accepts `agent`, `model`, and `variant` alongside `parts`, so
persona and model are selectable per turn without recreating a session.
**`model` must be an object**, `{"providerID": ..., "modelID": ...}`; a bare
`"digitalocean/kimi-k3"` string is rejected with `Expected object | null`.
Model identifiers may themselves contain slashes (`openrouter/z-ai/glm-5.2:free`),
so `provider/model` input splits on the first separator only.

**Model listing is `GET /config/providers`, not `GET /api/model`.** The latter
returns only opencode's own hosted catalog and omits every configured provider:
against this machine it listed 6 opencode models and zero of the 89 DigitalOcean
and 354 OpenRouter models actually reachable. Those hosted models are also not
usable without separate opencode Zen credentials, so a picker built on
`/api/model` would be both empty of real options and full of unusable ones.

A listed model is not necessarily a usable one. DigitalOcean returns
`this model is not available for your subscription tier` or `model not found`
for parts of its catalog, and the failure surfaces as an `UnknownError` from the
prompt route with the real cause only in the server log. The UI should report
these clearly rather than presenting the whole catalog as available.

### 5.3 Events consumed

`GET /event` is an SSE bus carrying all session activity. Consumed types:

| Event                  | Use                                               |
| ---------------------- | ------------------------------------------------- |
| `message.part.delta`   | Token deltas for live streaming                   |
| `message.part.updated` | Tool call state, citations                        |
| `message.updated`      | Token and cost accounting                         |
| `session.idle`         | **Turn complete** — the primary completion signal |
| `session.error`        | Surface failure, halt the debate                  |
| `permission.asked`     | Raise a permission card in the UI                 |
| `question.asked`       | Raise a question card in the UI                   |

Events are filtered by session id to route them to the correct column.

Every event nests its payload under a `properties` key. `message.part.delta`
carries its fragment in **`delta`**, not `text`, alongside a `field`
discriminator whose observed values are `text` for visible output and
`reasoning` for thinking tokens. Debate transcripts consume `text` only.

Decoding is deliberately tolerant, matching the philosophy in section 6: an
unrecognized event type, or a known type whose payload does not fit the
expected shape, degrades to an ignored event rather than terminating the
stream. opencode publishes many types coin does not model and adds more over
time, and one unmodelled event must never end a running debate.

### 5.4 Web search

Web search is the `websearch` tool, backed by Exa over MCP. It is **off by
default** and gated behind `OPENCODE_ENABLE_EXA=1` (aliases
`OPENCODE_EXPERIMENTAL=1`, `OPENCODE_EXPERIMENTAL_EXA=1`).

`EXA_API_KEY` is optional. Without it, opencode falls back to the keyless
`https://mcp.exa.ai/mcp` endpoint. `OPENCODE_WEBSEARCH_PROVIDER` accepts `exa`
or `parallel`.

Because this is experimental and flag-gated, tool availability is detected at
startup via `GET /experimental/tool` and the UI disables toggles for anything
absent rather than failing at first use.

### 5.4a A turn is not one message

`POST /session/{id}/message` returns **only the last assistant message** of a
turn. When a model uses tools, opencode records the turn as several assistant
messages, and the tool invocations, the reasoning, and part of the cost sit in
the earlier ones. A single verified turn looked like this:

```
assistant  [step-start, reasoning, text, tool(read), step-finish]
assistant  [step-start, text, step-finish]   <- only this is returned
```

Reading only the prompt response therefore loses every tool call and
undercounts cost. The engine re-reads `GET /session/{id}/message` after each
turn and aggregates every assistant message following the last user message.
This is deterministic and independent of event timing, which is why it is used
for the transcript rather than the event stream; events remain the mechanism
for live streaming.

`reasoning` is its own part type and is excluded from transcript text. It is
the model's internal monologue, not its argument.

### 5.4b A refused turn still returns 200

When a provider rejects the request, `POST /session/{id}/message` **succeeds**.
opencode records the failure on the assistant message instead: no text parts,
zero cost, zero tokens, and an `error` object on the message info.

```json
"error": {
  "name": "APIError",
  "data": {
    "message": "Rate limit exceeded: tokens_per_day.",
    "statusCode": 429,
    "isRetryable": true
  }
}
```

A client that only checks the HTTP status therefore sees a silent model rather
than a refused request, which is exactly wrong: it looks like a debater with
nothing to say. `MessageInfo::error` is read for this reason, and its
`isRetryable` flag is preferred over any judgement inferred from the status,
since the provider knows things the status code does not. `name` is the error
class, and a deliberate cancellation arrives as `MessageAbortedError` with an
empty `data`, so every field is optional.

opencode retries the provider itself, roughly six times with a widening
backoff, before recording the failure. Anything coin retries is therefore a
**second** layer on top of an upstream one that has already given up, which is
why coin's own retry count is small and why the provider's message is reported
rather than buried.

The rejection carries the provider's rate-limit headers in
`data.responseHeaders`, including remaining and reset values. Those header names
are provider-specific, so they are deliberately not parsed; the message text and
status are provider-neutral and say enough to act on.

### 5.5 Sessions and personas

Three sessions per debate: side A, side B, judge. Each keeps its own history, so
a debater sees its own prior reasoning plus exactly what the engine chooses to
show it of the opponent. The engine controls what crosses between them.

Personas are generated as agent markdown files in a per-debate workspace, which
is also the session project directory:

```
$XDG_DATA_HOME/coin/debates/<debate-id>/
  .opencode/agent/
    debater-a.md  debater-b.md    written before launch
  transcript.json  transcript.md  written when the debate ends
  .git/                           required; see section 5.1
```

A `judge.md` agent joins this in step 10, and a generated `opencode.jsonc`
carrying the permission policy of section 7 in step 9. Neither is written yet.

Generating agents here rather than in the user's own config keeps debate
personas from leaking into their normal opencode usage.

**Agent files are verified to control the persona.** A file at
`.opencode/agent/<name>.md` is registered under that name, appears in
`GET /agent`, and its body **replaces** the built-in system prompt rather than
being appended to it. This matters: without it, debaters would inherit
opencode's coding-assistant prompt and behave as coding agents. Confirmed by
giving an agent a distinctive instruction and observing the model follow it
exactly.

Agent files are read **when the server starts**, so every agent must be written
before launching. That is why the CLI prepares the workspace, writes both
personas, and only then launches opencode. When the web server arrives in step
6 and a single server outlives many debates, this needs revisiting: either the
server restarts per debate, or personas move into the first message of each
session at the cost of leaving the built-in prompt in place.

## 6. Prompt contract and structured extraction

Each format asks the model to close its turn with a fenced JSON block carrying
format-specific fields. For credence updating:

````
```json
{
  "credence": 64,
  "moved_because": "the 2024 figure I cited was superseded",
  "conceded": ["the effect size is smaller than I claimed"],
  "key_claim": "the causal direction runs the other way"
}
```
````

`debate::parse` extracts the last fenced `json` block and deserializes it.

**Parsing is deliberately tolerant.** A missing or malformed block degrades the
turn to prose-only, records a `ParseStatus`, and shows a warning badge in the
UI. It never fails the debate. This is the most likely source of runtime
friction, and it is cheaper to degrade than to halt.

## 7. Permissions and safety

Debaters can execute arbitrary shell commands. This is a deliberate capability
and the main reason the project is useful, but it is not something to leave
unsupervised by default.

Containment has two layers:

1. **Scoped workspace.** The session project directory is a disposable
   per-debate scratch directory, so file operations land somewhere throwaway
   rather than in the user's real projects.
2. **Ask by default.** The generated `opencode.jsonc` sets `ask` for `bash` and
   edit-class tools, `allow` for read-class tools and `websearch`. A
   `permission.asked` event becomes a card in the UI with allow and deny
   buttons. An **auto-approve** toggle rewrites the policy to `allow` for users
   who would rather not babysit it.

Neither layer is a sandbox. A debater that is allowed to run `bash` can reach
outside the workspace. Auto-approve should be treated as equivalent to running
an unsupervised agent with shell access, because that is what it is.

## 8. Engine

### 8.1 States

```
Idle -> Running -> Streaming{side} -> TurnComplete -> Running -> ... -> Judging -> Finished
             ^  |
             |  +-- Paused --> Resume
             |  +-- AwaitingPermission --> Resume
             +----- Reroll / Edit / Inject
```

`Failed` is reachable from any state on `session.error` or transport failure.

### 8.2 Commands

Delivered over an `mpsc` channel from the HTTP handlers:

`Start(DebateConfig)`, `Pause`, `Resume`, `Step`, `Inject { side, text }`,
`Reroll`, `EditTurn { index, text }`, `Abort`,
`RespondPermission { id, allow }`, `RespondQuestion { id, answer }`.

### 8.3 Intervention semantics

- **Pause** takes effect at the next turn boundary; the in-flight turn finishes
  streaming. **Abort** is the harder stop and cancels mid-token.
- **Inject** appends a user note to one side's session before its next turn.
- **Reroll** deletes the last assistant message and re-prompts.
- **Edit** replaces the turn text in the transcript and patches the part
  upstream, so the opponent sees the edited version.

The debate runs continuously by default and intervention acts on the last
completed turn. **Step mode** switches to pausing after every turn for close
supervision.

### 8.3a A silent turn is retried, then it stops the debate

A turn that comes back with no visible text is not a turn. Section 5.4b is why
this happens without an HTTP error, and the engine responds in three steps.

**Retry the transient.** Up to `RetryPolicy::attempts` attempts per turn, three
by default, with a backoff that starts at five seconds and doubles. A turn
already takes minutes, so the wait costs nothing next to the chance that an
overloaded provider clears. Each retry re-sends the same prompt, which appends
a fresh user message; since a turn is read back as everything after the **last**
user message, a retry cannot pick up debris from the attempt before it. Every
attempt is logged with the side, the attempt number, the delay, and the
provider's own words.

**Do not retry the permanent.** An exhausted account, a rejected key, or a model
the subscription does not cover will refuse identically however often it is
asked. Retrying those spends minutes to arrive at the same sentence, so they
fail on the first attempt and report the cause. The provider's `isRetryable`
flag decides, falling back to the status code.

**Stop the debate rather than continue without a side.** When a side is still
silent after every attempt, the debate ends with `StopReason::Failed { side,
message }`. The alternative, observed in a real run, is worse: the engine
carried on for four more rounds recording empty turns on both sides, spending
wall-clock time to produce a transcript whose convergence table was drawn from
two stale readings. Ending there preserves the turns already paid for, saves the
transcript as usual, names the side and the reason in it, and exits non-zero so
anything scripting coin can tell a failed debate from a finished one.

A turn with prose but no structured block is **not** retried. That is a model
ignoring the format, not a provider failing, and re-rolling it pays twice to
discard a real argument. It degrades to prose-only exactly as section 6 says.

### 8.4 Domain events

Broadcast to subscribers: `Snapshot`, `DebateStarted`, `RoundStarted`,
`TurnStarted`, `TurnDelta`, `ToolCallStarted`, `ToolCallCompleted`,
`TurnCompleted`, `AnalysisUpdated`, `PermissionRequested`, `QuestionAsked`,
`StateChanged`, `UsageUpdated`, `DebateFinished`, `Error`.

`Snapshot` carries the complete current state and is emitted to each subscriber
on connect. See section 9.3.

## 9. Coin's HTTP API

This is a **supported, scriptable interface**, not just the UI's private
backend. A debate can be configured, launched, steered, and exported entirely
from `curl` with no browser involved; the web UI is one client of this API
rather than a privileged one. Routes are resource-oriented and are expected to
stay stable.

### 9.1 Routes

| Method | Route                          | Purpose                                            |
| ------ | ------------------------------ | -------------------------------------------------- |
| GET    | `/`                            | UI, served from an embedded bundle                 |
| GET    | `/api/health`                  | Coin status, version, opencode child status        |
| GET    | `/api/models`                  | Available models, proxied from opencode            |
| GET    | `/api/tools`                   | Detected tool availability                         |
| GET    | `/api/formats`                 | The four formats and their default stop conditions |
| POST   | `/api/debate`                  | Create and start a debate                          |
| GET    | `/api/debate`                  | **Full state snapshot**                            |
| DELETE | `/api/debate`                  | Abort and clear the current debate                 |
| GET    | `/api/debate/turns`            | All turns; `?since=N` for the tail                 |
| GET    | `/api/debate/turns/{index}`    | A single turn                                      |
| PATCH  | `/api/debate/turns/{index}`    | Edit a turn's text                                 |
| POST   | `/api/debate/control`          | A command from section 8.2                         |
| GET    | `/api/debate/permissions`      | Pending permission requests                        |
| POST   | `/api/debate/permissions/{id}` | Allow or deny                                      |
| GET    | `/api/debate/questions`        | Pending questions                                  |
| POST   | `/api/debate/questions/{id}`   | Answer                                             |
| GET    | `/api/stream`                  | SSE of domain events, snapshot-first               |
| GET    | `/api/debates`                 | Previously saved debates                           |
| GET    | `/api/debates/{id}`            | A saved transcript                                 |
| GET    | `/api/transcript.json`         | Current debate, machine-readable                   |
| GET    | `/api/transcript.md`           | Current debate, human-readable                     |

Coin runs one debate at a time, which is why `/api/debate` is singular. Saved
debates are addressable under the plural collection.

### 9.2 Conventions

- All request and response bodies are JSON, except the Markdown export.
- Errors return an appropriate status with
  `{"error": {"kind": "...", "message": "..."}}`, produced by the single
  `AppError` type in `error.rs`.
- `GET /api/debate` returns `404` when no debate is active, so scripts can
  distinguish "no debate" from "debate idle".
- `GET /api/health` reports an API version. Breaking changes bump it; additive
  changes do not.

### 9.3 Snapshot-first streaming

`GET /api/stream` emits a `Snapshot` event carrying the complete current state
before any live events, then streams deltas.

This closes a real gap: a client that subscribes mid-debate, or a browser that
is simply reloaded, would otherwise render an empty view and populate only from
the next token delta. Replaying the snapshot on the stream itself, rather than
offering a separate endpoint the client must fetch first, removes the race
between fetching state and subscribing to changes. `GET /api/debate` still
exists for clients that want the state without holding a stream open.

### 9.4 Scripted example

```bash
curl -sX POST localhost:7777/api/debate -H 'content-type: application/json' -d '{
  "question": "Does the GIL still limit CPU-bound threading in Python 3.13?",
  "position_a": "The GIL remains a hard limit in default builds.",
  "position_b": "Free-threaded builds have removed the limit in practice.",
  "format": "credence",
  "max_rounds": 6
}'

curl -sN localhost:7777/api/stream          # snapshot, then live events
curl -s localhost:7777/api/transcript.md    # readable transcript
```

## 10. Web UI

Per `rust.md`: **Pico CSS and vanilla JavaScript only**. No React, no
jQuery, no component framework. Adaptive light and dark theming with an explicit
toggle. Google Fonts for a distinct header and body pairing. A custom stylesheet
rather than Pico defaults, themed to suit an adversarial-but-cooperative
subject. Served from a `rust-embed` bundle so the binary is self-contained.

- **Setup panel** — the inputs in section 2.1, with model pickers populated live.
- **Debate view** — two columns streaming tokens, turn cards showing tool
  invocations and citations inline. Since debaters have full tool access,
  showing what they ran and what came back is central to judging the reasoning,
  not a debug affordance.
- **Analysis rail** — format-dependent: convergence chart, claim ledger, or crux
  tree.
- **Intervention bar** — pause, resume, step, inject, reroll, abort, export,
  plus permission and question cards as they arrive.
- **Usage footer** — running token and cost totals, which arrive on every
  message response.

Per the guideline that deep computation stays in Rust, chart geometry (series
normalization, axis ticks, polyline points) is computed server-side and sent as
ready-to-render values. The JavaScript only emits inline SVG from them.

## 11. Layout and dependencies

The target layout. Entries marked _planned_ do not exist yet; see
`PROJECT_STATE.md` for what is built.

```
coin/
  Cargo.toml
  PROJECT_SPECS.md  PROJECT_STATE.md
  samples/config.toml       commented example settings; config.toml is ignored
  src/
    main.rs                 clap CLI: probe, debate
    config.rs               settings file, timeouts, data directory resolution
    error.rs                CoinError via thiserror
    term.rs                 terminal colour for the CLI
    store.rs                transcript persistence and export
    opencode/
      process.rs            spawn, port discovery, health poll, kill-on-drop
      client.rs             OpencodeClient trait, HttpClient
      events.rs             SSE consumer -> typed events
      types.rs              serde models for the REST surface
      workspace.rs          per-debate dir, git init, agent files
    debate/
      format.rs             DebateFormat trait, FormatId
      state.rs              DebateState, Turn, Credence, StopReason
      engine.rs             orchestrator
      parse.rs              tolerant fenced-JSON extraction
      credence.rs           the credence format
      crux.rs  classic.rs  ledger.rs      planned, step 7
      judge.rs                            planned, step 10
    web/                                  planned, step 6
      routes.rs             axum router + embedded static serving
      stream.rs             broadcast -> axum SSE
      api.rs                control handlers
  web/  index.html  app.js  styles.css    planned, step 6
```

In use today: `tokio`, `reqwest` (json, stream), `eventsource-stream`,
`futures-util`, `async-trait`, `serde`, `serde_json`, `toml`, `thiserror`,
`anyhow`, `tracing`, `tracing-subscriber`, `clap`, `chrono`, `dirs`, `dotenvy`,
`rand`, `uuid`.

Added with the web layer in step 6: `axum`, `tower-http` (trace, compression,
timeout), `rust-embed`.

## 12. Build order

1. This specification, reviewed before code.
2. Scaffold: `cargo init`, dependencies, `error.rs`, `config.rs`, tracing.
3. `opencode::process` and `client` — prove a session round trip.
4. `opencode::events` — consume `/event`, log deltas.
5. `debate::state`, the format trait, and **credence** only, end to end.
6. axum server, the section 9 API including snapshot-first streaming, and a
   minimal UI over it streaming that one format.
7. The remaining three formats behind the dropdown.
8. Intervention commands.
9. Permission and question cards.
10. Judge pass, persistence, export.
11. Analysis rail visualizations.
12. Optional, post-v1: serve a generated OpenAPI document at `/api/openapi.json`
    so the API is discoverable the way opencode's own is.

## 13. Verification

- `cargo test` covers `debate::parse` against deliberately malformed JSON
  blocks, each format's `should_stop`, and the engine state machine driven by a
  mocked client trait. No test contacts a real model.
- An integration test spawns a real `opencode serve`, creates a session, and
  asserts a completed message. Marked `#[ignore]` so the default run stays
  offline and fast.
- Integration tests pin an explicit cheap model rather than accepting the
  server default, which is a general-purpose model roughly 35 times more
  expensive. The default is `digitalocean/openai-gpt-oss-20b`, measured at
  about $0.0004 and one second per call against $0.0136 and several seconds;
  a full four-test run costs well under a cent and finishes in five seconds.
  `COIN_TEST_MODEL=provider/model` overrides it when a run needs different
  behaviour, such as a model that reliably invokes tools.
- API-level tests exercise every route in section 9 against an engine backed by
  a mocked client, including that `GET /api/stream` emits `Snapshot` before any
  live event and that a mid-debate subscriber reconstructs full state from it.
- `cargo clippy -- -D warnings` and `cargo fmt --check` clean.
- Manual end to end: run one debate per format on a question with a knowable
  answer; confirm live streaming, a tool call with citations rendered inline,
  pause, inject, reroll and resume all behaving, and a clean transcript export.
- Manual API check: drive a full debate from `curl` alone, with the browser
  closed, per the example in section 9.4.

## 14. Known risks

| Risk                                                          | Mitigation                                                                                                                                                                                                                                                                                           |
| ------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Models emit unparseable structure                             | Tolerant parser, degrade to prose, tested against malformed samples                                                                                                                                                                                                                                  |
| Debaters execute arbitrary commands                           | Scoped workspace, ask-by-default permissions, explicit auto-approve opt-in                                                                                                                                                                                                                           |
| Exa search is experimental and flag-gated                     | Startup capability detection, UI disables what is absent                                                                                                                                                                                                                                             |
| opencode API drift across releases                            | Integration isolated to `opencode/`, pinned version noted, ignored integration test catches breakage                                                                                                                                                                                                 |
| Cost accumulates quickly with tools                           | Live token and cost totals in the footer, hard round cap; tests pin a cheap model                                                                                                                                                                                                                    |
| Models vary in how well calibrated their stated confidence is | Observed during model selection: a candidate reported 20 percent confidence in a position the evidence plainly supported. A miscalibrated debater corrupts the convergence reading, so model choice is a correctness concern and not only a cost one, and the UI surfaces which model each side used |
| Debaters collapse into agreement without reasoning            | Prompts explicitly reward justified movement and penalize unjustified concession; the credence chart makes a suspicious collapse visible                                                                                                                                                             |
| A provider quota runs out mid-debate                          | Observed: a daily token limit turned every remaining turn into a silent one, and the debate ran on regardless. A refused turn returns 200 (section 5.4b), so the message error is read, transient failures are retried, and a side that cannot answer ends the debate with a stated reason (section 8.3a) |
