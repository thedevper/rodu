# ADR 0007: A card's reporter

- Status: accepted 2026-11-06

## Context

A card had an assignee, the person who does it, and nothing else about people. A team asked who
raised a card: who to ask when its description is unclear, and whose cards are waiting on others.
The answer was only in the card's history (ADR 0006), and only as long as the machine that created
the card was still on the team. An agent creating cards made it harder still: the change says the
person, and the agent only as a note on the change.

Every card now names its reporter. It is set when the card is made, people can change it like the
assignee, and it shows wherever the assignee does.

## Decision

### The field

A card has two new fields:

- `reporter_id`: the principal who reported the card, or none when nobody knows.
- `reporter_via_agent_id`: the agent it went through, if any, as a comment's `via_agent_id` is.

When a card is made without a reporter, the reporter is the person making it and the agent is the
one the write went through (`alice via alice-agent`). A reporter given by name or `me` is that
principal; an agent given as the reporter stands for its person. An agent, the actor's or the one
named, is kept only when the reporter is the actor's own person: no one is said to have used an
agent they did not. `none` (or null) makes a card with no reporter. Agents act for their person (ADR 0002), so
`reporter = me()` finds the cards a person's agent made.

Anyone who can edit a card can change or clear its reporter, as with the assignee, and the change is
activity like any other field (`reporter: ann → bob`). Naming the reporter a card already has changes
nothing, not even the agent, so the agent never changes without the reporter, which activity shows.

### Where it shows

- `rodu add --reporter <name|me>`; MCP `create_items` takes `reporter`, `update_item` takes
  `reporter` (null clears it), and its card summaries carry `reporter`.
- `rodu edit <key>... [--assignee <name|me|none>] [--reporter <name|me|none>]` changes who does,
  and who reported, one or more cards. It is the CLI's first command to edit a card; it takes only
  these two fields for now.
- `rodu show` and MCP `get_context` print `- reporter: ann via ann-agent` (`unknown` for none).
- `rodu ls` and `rodu edit` print the people after the card's line:
  `DEMO-1 [Todo] Title · assignee ann, reporter bob`.
- JQL-lite has `reporter`, like `assignee`: `reporter = me()`, `reporter IS EMPTY`.
- The web API's card has `reporter` and `reporter_via`. The board shows "by ann" on each card, and
  the card panel has a Reporter choice beside the Assignee one.
- The readable copy of the board (ADR 0004) lists `Reporter` under `Assignee`.

### Storage and migration

The local index goes to schema 3. `items` gets `reporter_id` and `reporter_via_agent_id`, both
referring to `principals`, added with `ALTER TABLE` (a schema 1 file is rebuilt with them already, so
the migration adds them only when missing), and `reporter_id` gets an index like `assignee_id`.

The migration then gives each card the person and agent of the first `item.create` event this
machine recorded for it, when that person is still a principal, and raises the card's version. A
card with no such event keeps no reporter. Nothing is guessed: a card is never given the person who
happens to run the upgrade.

### Sync

In a team the card's map in the document holds `reporter_id` and `reporter_via_agent_id`, written
like every other field: only when they change. A reporter that names nobody the index knows is
cleared in the index and reported as unsettled, as an unknown assignee is. The agent is kept in the
index only when it is an agent of the reporter, so a document that pairs one person with another's
agent shows the person alone (a change's agent is shown the same way, ADR 0006).

Cards made before this version have no reporter in the document. Each machine, at the first command
of a Rodu with this ADR that pulled from the team folder, fills in the cards whose map has no
`reporter_id` key at all (a reporter cleared on purpose is a null value, and stays cleared). It
waits for a pull so a reporter a teammate already set is there to keep, rather than racing it:

1. the person whose admitted machine created the card in the team's history (ADR 0006), with the
   agent its change names; else
2. the person and agent of the `item.create` event this machine recorded for the card.

A card the team board was made with (`item.adopt`) was created before the team existed, so only
the machine that made the board knows its reporter, from its own events. The others leave it to that
machine. Every machine reads the same history first, so two machines filling in the same card write
the same value.

The fill-in writes keys of its own, `reporter_found_id` and `reporter_found_via_agent_id`, never
`reporter_id`. A card's reporter is `reporter_id` when the map has that key, set or cleared, and the
found one only when it has not. So a reporter someone set wins over a fill-in whatever order the
two arrive in, even when a machine fills in before it hears of a teammate's change. A card whose
index already holds a reporter from its own migration, and whose history names no one, takes that
one, so the index and the document agree.

The fill-in is one change whose message is `<parity> backfill`. Activity does not know the found
keys, so the fill-in shows there as nothing, and the word `backfill` hides nothing else: a change
that says it and edits a field is shown like any change. `rodu audit` lists it. A later change to
a filled-in reporter shows in activity from none, since that is what the document's
`reporter_id` held. A local note
(`reporters-backfilled`), read and written under the write lock, keeps it to once per machine, so a
card that arrives later without a reporter (written by an older Rodu) keeps none until someone sets
it. A fill-in that fails is a warning and leaves the note unset, so the next command tries again; it
never stops the command.

### Older versions

Rodu 0.9 and older read cards field by field and ignore fields they do not know, and they write only
the fields that changed, so they neither drop nor break a reporter. Cards they create have no
reporter. Their activity does not list the reporter field, and they read a `backfill` message as
one with no parity digit, whose entry has no field they show, so it shows nothing.

## Consequences

- People can ask "what did I report" (`reporter = me()`) and see who raised a card at a glance.
- Every surface that lists the assignee now lists the reporter, which makes `rodu ls` lines longer.
- A card may have no reporter for good: made before this version on a machine that has left the
  team, made by an older Rodu, or cleared. That is shown as unknown, never filled in with a guess.
- The fill-in writes to the team document once per machine that upgrades. On a large board that is
  one change touching many cards, but it holds two short fields per card. A machine with no events of
  its own reads each card's history once; a board of a few hundred cards took under a second.
- An agent's own name is not the reporter. Per-agent identity, if it comes, is a separate decision.

## Not done

- Notifications or a "my reported cards" view on the board: JQL covers it for now.
- Restricting who may change a reporter: anyone who can edit a card can, as with the assignee.
- Editing other fields from the CLI: `rodu edit` takes the assignee and the reporter only.
