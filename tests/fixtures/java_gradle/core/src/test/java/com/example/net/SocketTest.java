package com.example.net;

import com.example.net.kinds.Stream;
import com.example.net.socket.Socket;

public class SocketTest {
    boolean accepts() {
        Stream stream = Socket.listen(4);
        return stream.accept();
    }
}
