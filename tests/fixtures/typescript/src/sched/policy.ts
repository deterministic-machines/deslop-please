import { Ring } from '@/queue/ring';

export function admit(ring: Ring): void {
  ring.items = [];
}

export function next(ring: Ring): number {
  const item = ring.head;
  ring.head = ring.head + 1;
  return item;
}
