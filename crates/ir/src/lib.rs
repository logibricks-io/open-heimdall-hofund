//! Canonical in-memory types for an LLM request, a response, and a stream event.
//! This crate is pure data: it performs no I/O and names no vendor. Every protocol
//! codec translates into and out of these types so the rest of hofund speaks one dialect.
