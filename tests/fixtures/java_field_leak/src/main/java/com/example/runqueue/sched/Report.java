package com.example.runqueue.sched;

import com.example.runqueue.build.Mode;
import com.example.runqueue.queue.RunQueue;
import com.example.runqueue.queue.Status;

public final class Report {
    public static int backlog(RunQueue queue) {
        return queue.items.size() - queue.getHead();
    }

    public static boolean drained(RunQueue queue) {
        return queue.head == queue.items.size();
    }

    public static Mode modeOf(RunQueue queue) {
        return queue.mode;
    }

    public static boolean idle(RunQueue queue) {
        return queue.status == Status.IDLE;
    }

    public static String name() {
        return RunQueue.class.getSimpleName();
    }
}
