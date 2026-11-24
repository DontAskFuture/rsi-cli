# Event Filter Matcher Spec

Optimize `filterAllowedEvents(events, blockedIds)` in `src/matcher.ts`.

## Requirements
- Return a new array containing all `events` whose `id` does not appear in `blockedIds`.
- Preserve exact input ordering of `events`.
- Never mutate `events` or `blockedIds`.
