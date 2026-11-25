package com.example.runqueue.app;

import com.example.runqueue.queue.RunQueue;
import com.example.runqueue.sched.Policy;
import com.example.runqueue.sched.Report;

public class App {
    public static int run(int[] tasks) {
        RunQueue ready = RunQueue.fresh(4);
        int ran = 0;
        for (int task : tasks) {
            if (!Policy.admit(ready, task)) ran += drain(ready);
        }
        return ran + Report.backlog(ready);
    }

    private static int drain(RunQueue ready) {
        int ran = 0;
        while (!Report.drained(ready)) {
            if (Policy.next(ready) != null) ran += 1;
        }
        return ran;
    }
}
