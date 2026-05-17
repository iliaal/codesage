package com.acme.pipeline;

import com.acme.support.BaseProcessor;
import java.io.Closeable;

interface Processor<T extends Comparable<T>> extends AutoCloseable {
    void process(T input);
}

class StringProcessor extends BaseProcessor implements Processor<String>, Closeable {
    private String lastValue;

    public StringProcessor() {
        super();
    }

    public void process(String input) {
        this.lastValue = input.trim();
        emit(lastValue);
    }

    private void emit(String value) {
        System.out.println(value);
    }
}
