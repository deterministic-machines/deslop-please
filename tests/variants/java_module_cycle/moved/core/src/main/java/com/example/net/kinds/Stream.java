package com.example.net.kinds;

public final class Stream {
    public static final int BACKLOG_MOST = 16;

    private final int backlog;
    private int pending;

    private Stream(int backlog) {
        this.backlog = backlog;
    }

    public static Stream open(int backlog) {
        return backlog <= BACKLOG_MOST ? new Stream(backlog) : null;
    }

    public boolean accept() {
        boolean room = pending < backlog;
        if (room) {
            pending += 1;
        }
        return room;
    }
}
