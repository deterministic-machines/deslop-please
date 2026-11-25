package com.example.runqueue.queue;

import com.example.runqueue.build.Mode;
import java.util.ArrayList;
import java.util.List;

public class RunQueue {
    public final List<Integer> items = new ArrayList<>();
    public int head;
    public Mode mode = new Mode();
    public Status status = Status.IDLE;
    private final int limit;

    public RunQueue(int limit) {
        this.limit = limit;
    }

    public boolean full() {
        status = Status.BUSY;
        return items.size() - head >= limit + mode.level;
    }

    public int getHead() {
        return head;
    }

    public static RunQueue fresh(int limit) {
        return new RunQueue(limit);
    }
}
