package com.example.net.socket;

import com.example.net.kinds.Stream;

public final class Socket {
    public static Stream listen(int backlog) {
        return Stream.open(backlog);
    }
}
