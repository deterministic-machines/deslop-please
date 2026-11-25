package com.example.app;

import com.example.net.socket.Socket;

public class Main {
    public static void main(String[] args) {
        System.out.println(Socket.listen(4).accept());
    }
}
