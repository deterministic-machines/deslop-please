import { Ring } from './queue';

export function tick(): number {
  const ring = new Ring();
  return ring.shown();
}
