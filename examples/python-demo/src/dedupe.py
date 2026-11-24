from typing import List


def dedupe_preserve_order(items: List[str]) -> List[str]:
    """Return unique items while preserving their first-seen order."""
    result: List[str] = []
    for item in items:
        if item not in result:
            result.append(item)
    return result
