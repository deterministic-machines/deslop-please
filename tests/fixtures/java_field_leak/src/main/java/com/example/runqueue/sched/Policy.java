package com.example.runqueue.sched;

import com.example.runqueue.queue.RunQueue;

public final class Policy {
    public static boolean admit(RunQueue queue, int task) {
        if (queue.full()) {
            return false;
        }
        queue.items.add(task);
        return true;
    }

    public static Integer next(RunQueue queue) {
        if (queue.head >= queue.items.size()) return null;
        Integer task = queue.items.get(queue.head);
        queue.head += 1;
        return task;
    }
}
