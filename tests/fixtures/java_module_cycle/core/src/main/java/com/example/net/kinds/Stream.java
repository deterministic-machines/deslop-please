package com.example.net.kinds;

import com.example.net.socket.Socket;

public final class Stream {
    private final int backlog;
    private int pending;

    private Stream(int backlog) {
        this.backlog = backlog;
    }

    public static Stream open(int backlog) {
        return backlog <= Socket.BACKLOG_MOST ? new Stream(backlog) : null;
    }

    public boolean accept() {
        boolean room = pending < backlog;
        if (room) {
            pending += 1;
        }
        return room;
    }
}
