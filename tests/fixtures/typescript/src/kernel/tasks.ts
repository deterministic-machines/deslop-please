import { Timers } from './timers';

export class Tasks {
  schedule(timers: Timers): void {
    timers.arm();
  }
}
