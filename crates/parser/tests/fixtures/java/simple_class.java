package com.acme.service;

import java.util.List;

class SimpleClass {
    private final List<String> names;

    public SimpleClass(List<String> names) {
        this.names = names;
    }

    public void addName(String name) {
        names.add(name);
        log(name);
    }

    private void log(String value) {
        System.out.println(value);
    }
}
