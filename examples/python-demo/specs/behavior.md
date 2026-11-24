# Order-Preserving Deduplicator Spec

Optimize `dedupe_preserve_order(items)` in `src/dedupe.py`.

## Requirements
- Input: `list[str]`
- Output: a new `list[str]` containing only the first occurrence of each distinct element in original order.
- Must not mutate the input list.
- Must remain pure Python standard library only.
