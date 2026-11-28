import { Tasks } from './tasks';

export class Timers {
  fire(tasks: Tasks): void {
    tasks.wake();
  }

  arm(): void {}
}
