import { Ring } from '@/queue/ring';

export function backlog(ring: Ring): number {
  return ring.items.length - ring.head;
}
