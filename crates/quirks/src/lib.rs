//! Data-driven descriptions of vendor capabilities and known protocol deviations:
//! which parameters an upstream accepts, which limits it enforces, and where its
//! behavior differs from the protocol's usual reading. Provider adapters consult
//! this data. Gateway and router code must not branch on vendor names.
