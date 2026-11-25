package com.example.net.socket;

import com.example.net.kinds.Stream;

public final class Socket {
    public static final int BACKLOG_MOST = 16;

    public static Stream listen(int backlog) {
        return Stream.open(backlog);
    }
}
