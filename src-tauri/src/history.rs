//! Query history recording (`history_list` reads the store directly; writing a history entry
//! around every `query_execute` / `lease_execute` call is added here).
//!
//! Owner: package B (editor backend). Scaffold: intentionally empty, `ExecuteResult::history_id`
//! stays `None` until then.
