package com.acme.nested;

class Outer {
    private Helper helper;

    Outer() {
        this.helper = new Helper();
    }

    void run() {
        helper.work();
    }

    static class Inner {
        void visit() {
            new Helper().work();
        }
    }

    interface Marker {
        void mark();
    }

    enum Mode {
        FAST,
        SLOW
    }
}

class Helper {
    void work() {
    }
}
