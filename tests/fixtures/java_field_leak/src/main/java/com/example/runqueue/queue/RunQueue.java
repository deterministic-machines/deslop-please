package com.example.runqueue.queue;

import java.util.ArrayList;
import java.util.List;

public class RunQueue {
    public final List<Integer> items = new ArrayList<>();
    public int head;
    private final int limit;

    public RunQueue(int limit) {
        this.limit = limit;
    }

    public boolean full() {
        return items.size() - head >= limit;
    }

    public int getHead() {
        return head;
    }

    public static RunQueue fresh(int limit) {
        return new RunQueue(limit);
    }
}
