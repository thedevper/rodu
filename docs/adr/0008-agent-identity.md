# ADR 0008: Each agent has a name, and a card keeps who created it

- Status: accepted 2026-11-07

## Context

Every person has one agent principal, `<name>-agent`, made by `rodu init` and `team join`
(ADR 0002). `rodu mcp` wrote as that agent; the CLI never named an agent at all. So:

1. **An agent in a shell was the person.** An agent that runs `rodu add` from its shell, which is
   how most coding agents use a CLI, was recorded as the person typing it. Activity, audit and the
   reporter (ADR 0007) could not tell the two apart.
2. **Every agent was the same agent.** Claude Code, a desktop client and a script all wrote as
   `ann-agent`, so "what did this agent do" had no answer.
3. **Nothing said who made a card.** The reporter starts as the creator, but anyone can change
   it, and the history that says who made a card is local to each machine (events) or a full read
   of the team history (ADR 0006). There was no way to ask "the cards an agent made" in a query.

## Decision

### An agent of its own, under its person

An agent's name is `<person>-<agent>`: the agent part lowercased, letters, digits, dots and
underscores kept and every run of other characters made one dash, the whole cut to 40 characters
(a principal name's limit). `claude.code` and `claude-code` are two agents. It is made the first time
it writes, as an agent owned by the person, and found by name after that.

- **`rodu mcp`** uses `RODU_AGENT` when it is set, else the name the MCP client gives in its
  `initialize` request (a client named `claude-code` makes `ann-claude-code`). The agent is settled on the first
  tool call and kept for the session. A client name that cannot make a name (no letter or digit, or
  a name a human or another person's agent has) falls back to the person's usual agent, as before;
  a bad `RODU_AGENT` fails each tool call instead, so a wrong setting is seen.
- **The CLI** writes as an agent when the environment says one runs it: `RODU_AGENT`, else
  `claude-code` when `CLAUDECODE=1`, which Claude Code sets in the shells it starts. A person at a
  terminal sets neither and is recorded as the person, as before. Every item command but the
  ones that only read (`ls`, `show`, `audit`, `collections`) makes the agent, after the team pull,
  before it checks its arguments: a write command that then fails may still have made it.
- **The id comes from the person and the name**: the first 16 bytes of
  SHA-256(`agent:<person id>:<lowercased name>`) as a version 8 UUID. Two machines of one person
  that both make `ann-claude-code` before they sync make the same principal, which merges into
  one, rather than two principals with one name.
- An agent is per name, not per process or per session: one id per agent keeps `team members`
  short and lets `creator = ann-claude-code` mean that agent across sessions.

The change itself is the one ADR 0002 already allows: any member may add a principal, and a change
names an agent only when that agent is owned by the writing machine's person (ADR 0006), which a
person's own named agent is. Older Rodu versions read the new agents like any other.

### A card keeps who created it

A card has two new fields, `creator_id` and `creator_via_agent_id`: the actor, and the agent it
went through, when the card was made. They are set once and never edited, unlike the reporter.

- `rodu show` and MCP `get_context` print `- created by: ann via ann-claude-code` (`unknown` for
  none).
- JQL-lite has `creator`. `creator = ann` matches the cards ann made, through any agent or none;
  `creator = ann-claude-code` matches the cards that agent made, since an agent's id is never a
  person's. `creator != x`, `IN`, `NOT IN` and `IS EMPTY` work as on other fields: `=` and `IN`
  with a name nobody has match nothing (not the cards whose creator is unknown), and `!=` and
  `NOT IN` keep the cards with no creator, as `!=` keeps empty fields elsewhere.

### Storage, migration and sync

- The local index goes to schema 4: `items` gets the two columns (added with `ALTER TABLE` when
  missing) and an index on `creator_id`. The migration fills them from the first `item.create`
  event this machine recorded for each card, when its person is still known, as schema 3 did for
  the reporter, but without raising the card's version: nobody edited it. The team fill-in below
  does not raise it either.
- In a team the card's map holds `creator_id` and `creator_via_agent_id`, written when the card is
  made. Activity does not list them (ADR 0006's list of fields is unchanged).
- Cards made before this version have no `creator_id` key. Each machine, after a pull, fills them
  in once, exactly as it fills in reporters (ADR 0007): from the person whose admitted machine
  created the card in the team history, with its change's agent, else from its own events, into
  keys of their own (`creator_found_id`, `creator_found_via_agent_id`) under a local note
  (`creators-backfilled`), in a change whose message is `<parity> backfill`. Nobody sets a creator
  by hand, so the found keys never race a person's edit; they only have to agree between machines,
  which they do because every machine reads the same history first.
- A creator, or its agent, the index cannot resolve is cleared there as the reporter is.
- As with reporters, a card an older Rodu creates after this machine's fill-in has no creator
  here until someone with this version made it; it shows as unknown.

## Consequences

- An agent's work is told apart from its person's everywhere a change names an agent: activity,
  audit, the reporter, and now the creator.
- `team members` lists one more agent per agent name a person uses.
- A creator is what the card's map says, like the reporter and a comment's author: a member who
  changes their Rodu can write any value there. Activity and audit, read from the team's history,
  which names the machine behind each change, stay the record to check it against (ADR 0006).
- A person whose name is 39 or 40 characters long has no room left for an agent's name: the CLI
  refuses `RODU_AGENT` and MCP keeps the usual agent.
- Two agent names that are the same in their first 40 characters, with the person's name, are one
  agent. Agent names are short in practice; a longer one is better set short with `RODU_AGENT`.
- Detecting an agent in a shell relies on the agent saying so. Claude Code does by itself; others
  need `RODU_AGENT` set in their environment. A person who runs `rodu` from inside a Claude Code
  shell by hand (`! rodu add ...`) is recorded as `via ann-claude-code`.
- A card made by a Rodu older than this one, on a machine that has since left the team, may have
  no creator for good. That shows as unknown, never as a guess.

## Not done

- Proving which agent wrote (signing per agent): `via` stays the writing machine's word (ADR 0006).
- Renaming or retiring an agent.
- Restricting what an agent may do; that is a separate decision.
