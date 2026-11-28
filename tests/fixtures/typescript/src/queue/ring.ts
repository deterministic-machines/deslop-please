import { tick } from '@/clock';

export class Ring {
  items: number[] = [];
  head: number = 0;
  limit: number = 8;

  push(value: number): void {
    this.items = [...this.items, value + tick()];
  }

  shown(): number {
    return this.items.length;
  }

  full(): boolean {
    return this.items.length - this.head >= this.limit;
  }
}
