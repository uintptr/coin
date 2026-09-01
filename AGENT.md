# Agent Guidelines

@AGENT_rust.md

## Project documents

**Read @PROJECT_STATE.md first.** It is the project's memory: what is built,
what is left, what surprised us, and where to pick up. Start every session
there.

@PROJECT_SPECS.md is the design: what we are building and why. Every design
decision lives there.

The division of labour: the specs answer "what should this be and why", the
state answers "where did we get to". Neither duplicates the other; the state
file links to spec sections rather than restating them.

## Keeping them current

These are not documentation chores. They are the only thing that survives a
session ending, so treat them as part of the work rather than as cleanup.

**Any design decision must be mirrored into `PROJECT_SPECS.md` as part of the
same change that acts on it.** This applies equally to decisions the user
states, decisions reached together, and decisions forced by something discovered
about an upstream dependency. A decision agreed in conversation and not written
down is lost the moment the session ends.

**`PROJECT_STATE.md` is updated in the same commit as the work it describes.**
Mark steps done, record new open questions, delete answered ones, and leave
"Next session: start here" specific enough to act on without rereading the code.
Its own "Updating this file" section says exactly what to touch and when.

Prefer deleting stale content over accumulating it. A section that is no longer
true is worse than an absent one.
