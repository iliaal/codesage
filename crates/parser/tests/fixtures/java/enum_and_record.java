package com.acme.model;

import java.time.Instant;

enum Status {
    ACTIVE,
    DISABLED;

    public boolean active() {
        return this == ACTIVE;
    }
}

record AuditEvent(String id, Instant at) implements DomainEvent {
    public AuditEvent {
        id.trim();
    }

    public String label() {
        return new StringBuilder(id).append(at).toString();
    }
}

interface DomainEvent {
}
