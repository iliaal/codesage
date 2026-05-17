package com.acme.spring;

import org.springframework.stereotype.Component;
import org.junit.jupiter.api.Test;

@FunctionalInterface
public @interface MyMarker {
    String value() default "x";
}

@Component("svc")
@Deprecated
class AnnotatedService {
    private String firstName, lastName, email;

    @Override
    @Test
    public void run() {
    }
}
