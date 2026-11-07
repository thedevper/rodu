# ADR 0009: Cards an agent creates wait in Triage

- Status: accepted 2026-11-07

## Context

Since ADR 0008 an agent writes as an agent of its own, and a card keeps who created it. An agent
can create many cards quickly, and they went straight into Backlog next to the cards people
chose to keep. Nothing asked a person whether an agent's card belongs on the board.

## Decision

### A Triage status

Every collection's workflow has a `Triage` status in the backlog category, before `Backlog`.

- A card created through an agent (`creator_via_agent_id` set) starts in `Triage`; a card a
  person creates starts in the workflow's initial status (`Backlog`), as before.
- Collections made before this version have no `Triage` in their stored workflow. Rodu adds it
  when it reads a collection rather than rewriting the stored workflow, so no migration, local or
  in a team, touches a collection. A stored workflow that already has a `Triage` status is kept
  as it is.
- `Triage` is in the backlog category, so reports, cycle counts and older Rodu versions treat a
  waiting card as backlog. An older version does not know the status: it can still move a card
  out of it, through the transitions every status has (`*`).
- No transition leads into `Triage`: a card only starts there.

### `rodu triage` and the `triage` MCP tool

- `rodu triage [--collection KEY]` lists the cards waiting, oldest first, without the snoozed
  ones: at most 100, and it says how many more wait.
- `rodu triage accept <key>...` moves cards to the workflow's initial status (`Backlog`).
- `rodu triage decline <key>...` moves them to `Canceled`.
- `rodu triage duplicate <key> <of>` links the card as a duplicate of `<of>` and moves it to
  `Canceled`, in one write. A `duplicates` link to `<of>` made earlier is kept, not refused.
- `rodu triage snooze <key>... --days N` (1 to 365) hides cards from the list until that date.
  They stay in `Triage`; a later accept or decline still works, and snoozing again replaces the date.
- Each of them works only on a card in `Triage`, and refuses others with a hint, so a mistyped
  key cannot cancel a card on the board.
- The MCP tool `triage` takes `action` (`list`, `accept`, `decline`, `duplicate`, `snooze`) with the
  same rules. It is recorded like any other change: by the person, via the agent.

### Storage

- A card has a new field, `snoozed_until` (a date, `YYYY-MM-DD`), which only snooze sets.
- The local index goes to schema 5: `items` gets the column (added with `ALTER TABLE` when
  missing). In a team, the card's map holds `snoozed_until`; older versions ignore the key.
- A card that leaves Triage by any path (a decision, `rodu mv`, the board, an update) loses
  `snoozed_until`: only a card waiting in Triage sleeps.
- The date shows as `snoozed` in a card's activity and in the MCP card view, so a person or an
  agent can see when a snoozed card comes back.

## Consequences

- An agent's cards wait for a person; the board's `Triage` column (the web view draws one column
  per status) shows them. It has no "+" button: a person's new card goes to the other columns.
- A person working through Claude Code is an agent too (ADR 0008): cards created that way wait
  in Triage like any agent's. Accepting them is one command.
- Nothing stops an agent from moving its own card out of `Triage` with `mv` or `transition`:
  Triage sorts work, it is not a permission. The change is recorded via the agent.

## Not done

- Restricting who may triage (people only).
- Turning Triage off for a collection.
- Waking a snoozed card with a notification: it simply shows in the list again.
