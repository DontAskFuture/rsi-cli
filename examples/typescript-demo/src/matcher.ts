export interface EventItem {
  id: string;
  score: number;
}

/**
 * Return events whose id is not in blockedIds, preserving original order.
 */
export function filterAllowedEvents(events: EventItem[], blockedIds: string[]): EventItem[] {
  return events.filter((ev) => !blockedIds.includes(ev.id));
}
