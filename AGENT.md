# Agent Guidelines

@AGENT_rust.md

## Project documents

Read both before starting work.

- @PROJECT_SPECS.md — the design. What we are building and why. Every design
  decision lives here, including ones made mid-conversation.
- @PROJECT_STATE.md — the memory. What is built, what is left, what surprised
  us, and where to pick up next.

## Keeping them current

**Any design decision must be mirrored into `PROJECT_SPECS.md` as part of the
same change that acts on it.** A decision agreed in conversation and not written
down is lost the moment the session ends. This applies equally to decisions the
user states, decisions reached together, and decisions forced by something
discovered about an upstream dependency.

`PROJECT_STATE.md` is updated as work moves: mark steps done, record new open
questions, and keep the "start here" section pointing at the real next task.

The division of labour: `PROJECT_SPECS.md` answers "what should this be and
why", `PROJECT_STATE.md` answers "where did we get to". Neither duplicates the
other; the state file links to spec sections rather than restating them.
