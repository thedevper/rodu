# 10. Stale policy: `rodu tidy`

Status: accepted

## Context

A backlog fills with cards nobody will do. Agents make this worse, since they write cards
faster than a person reads them. A person needs a way to find the cards no one has touched for a
while, and then to clear them. That should not mean losing a card someone still cares about, or
cards changing without a record of why.

## Decision

### Rules are data

The rules live in `.rodu/tidy.json`, next to the workspace's config. They are not code and not
flags:

```json
{"rules": [
  {"name": "nudge", "collection": "DEMO", "stale_after": "30d", "action": "comment"},
  {"name": "close", "stale_after": "60d", "exempt": "priority = urgent", "action": "close"}
]}
```

- `name` is 1 to 20 lowercase letters, digits or dashes, and is unique in the file.
- `collection` is optional. Without it, a rule covers every collection.
- `stale_after` is a number of days, from `1d` to `3650d`.
- `exempt` is optional JQL for more cards to leave alone. The cards it matches are left out;
  a card that leaves a field empty does not match a test on that field, so
  `assignee = bob` leaves out bob's cards and keeps the unassigned ones in the sweep.
- `action` is one of:
  - `flag` lists the card;
  - `comment` adds one comment;
  - `close` adds a comment and moves the card to `Canceled` (or the first done status).
- Unknown keys are refused, so a typo cannot silently turn a rule off.

### What is stale

A card is stale when all of the following hold:

- it is in the backlog category;
- its last activity is older than the rule's age;
- nothing exempts it.

Last activity is the latest of two things: the card's last change and its last comment. Comments
written by any tidy rule do not count, so a nudge does not keep a card fresh for the close rule.
That covers every agent named `<person>-tidy-<rule>`: another person's rules, and a rule since
renamed or removed from the file.

These are always exempt, whatever the rule says:

- a card with a due date;
- a card in a cycle;
- a card that is blocked, meaning a card not yet done blocks it;
- a card in Triage. Triage has its own lifecycle (ADR 0009).

A snoozed card is only ever in Triage, so it is exempt until it wakes and someone decides on it.
Each rule also reports how many cards have been waiting in Triage for longer than its age, as a
separate line that points to `rodu triage`.

The exemptions are checked in code on every card, not only in the generated JQL. The `exempt` is
run on its own first, so a broken one stops the run. It can only leave cards out, never sweep in
a done card or a card with a due date.

### Dry run first, and the rule as the actor

- `rodu tidy` is a dry run. It prints what each rule would do and writes nothing, not even the
  rule's agent.
- `rodu tidy --apply` does it. `--dry-run` is accepted, and passing both is an error. Other
  commands refuse both flags, so `--dry-run` never silently does the real thing.
- When applied, each rule writes as its own agent, `<you>-tidy-<name>`, owned by the person who
  runs it. The card's history then says the rule did it, through which person. This holds even
  when an agent is the one running `rodu tidy`.

### Safe to run again, and stops cleanly

- A `comment` rule does not comment twice. A card it already commented on since the card's last
  activity is listed as "already commented on", and left alone.
- A `close` rule moves the card out of the backlog, so a second run does not find it.
- Every rule is checked and planned before anything is written. A bad `exempt`, an unknown
  collection, two rules that would share an agent name, or a close the workflow refuses stops
  the whole run with nothing changed.
- When a rule writes, it reads each card again and skips one that changed since the plan, by a
  person or by an earlier rule. Two close rules that match the same card close it once.
- When applied, each rule's changes are one write. A rule that fails partway changes nothing, and
  the error names the card it stopped at and the rules already applied.
- There is no cap or throttle: the store is local.

## Consequences

- A card's comments now matter for staleness, so each candidate costs a comment read. A backlog
  of a few thousand cards is still quick.
- Closing a card can unblock the cards it blocked. Those cards are stale on the next run, not the
  same one: a run acts only on what it planned before writing.
- A card past both a nudge rule's age and a close rule's age is nudged and closed in the same
  run. The nudge then gives no warning. Set the close age well above the nudge age, so a card
  that was nudged has time to answer.
- The rules are per workspace folder and are not synced. In a team, the person who runs
  `rodu tidy` uses their own rules.

## Not done

- A `snooze` action. Snooze exists only in Triage, which tidy leaves alone, and a backlog card
  cannot sleep (ADR 0009).
- Labels, such as `label:pinned`. Rodu has no labels. Until it does, a rule's `exempt` JQL does
  the job, for example `priority = urgent`.
- Running on a schedule, and an MCP tool. `rodu tidy` is run by a person for now.
