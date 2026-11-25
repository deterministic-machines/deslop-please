package com.example.runqueue.sched;

import com.example.runqueue.queue.RunQueue;

public final class Report {
    public static int backlog(RunQueue queue) {
        return queue.items.size() - queue.getHead();
    }

    public static boolean drained(RunQueue queue) {
        return queue.head == queue.items.size();
    }
}
