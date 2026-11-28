import { Ring } from '@/queue';
import { tick } from '@/clock';

export function start(ring: Ring): number {
  ring.push(tick());
  return ring.shown();
}
