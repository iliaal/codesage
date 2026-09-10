use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::Instant;

use parking_lot::Mutex;
use serde_json::{Value, json};

const DETAIL_LIMIT: usize = 256;
const BYTE_LIMIT: usize = 2 * 1024 * 1024;
const PROJECT_LIMIT: usize = 1024;
const HISTOGRAM_MS: [u64; 10] = [1, 5, 10, 50, 100, 500, 1_000, 5_000, 25_000, u64::MAX];

fn tool_name(value: &str) -> &'static str {
    match value {
        "project_overview" => "project_overview",
        "search" => "search",
        "find_symbol" => "find_symbol",
        "find_references" => "find_references",
        "find_similar" => "find_similar",
        "list_dependencies" => "list_dependencies",
        "trace_call_path" => "trace_call_path",
        "from_trace" => "from_trace",
        "impact_analysis" => "impact_analysis",
        "export_context" => "export_context",
        "find_coupling" => "find_coupling",
        "assess_risk" => "assess_risk",
        "assess_risk_batch" => "assess_risk_batch",
        "assess_risk_diff" => "assess_risk_diff",
        "recommend_tests" => "recommend_tests",
        "review_rehearsal" => "review_rehearsal",
        "session_start" => "session_start",
        "session_end" => "session_end",
        "list_features" => "list_features",
        "find_feature" => "find_feature",
        "feature_bundle" => "feature_bundle",
        "edit_check" => "edit_check",
        "embed_texts" => "embed_texts",
        "rerank_pairs" => "rerank_pairs",
        "overview_ranking" => "overview_ranking",
        "overview_generation" => "overview_generation",
        _ => "unknown",
    }
}

fn outcome_name(value: &str) -> &'static str {
    match value {
        "success" => "success",
        "error" => "error",
        "timeout" => "timeout",
        "cancelled" => "cancelled",
        "saturated" => "saturated",
        "shutdown" => "shutdown",
        "incomplete" => "incomplete",
        "database-busy" | "database_busy" => "database-busy",
        "panic" => "panic",
        "abandoned" => "abandoned",
        _ => "unknown",
    }
}

fn phase_name(value: &str) -> &'static str {
    match value {
        "queued" => "queued",
        "running" => "running",
        "native" => "native",
        "shared_wait" => "shared_wait",
        _ => "unknown",
    }
}

fn bounded_project(value: &str) -> String {
    let mut end = value.len().min(PROJECT_LIMIT);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

fn millis(start: Instant, end: Instant) -> u64 {
    end.duration_since(start).as_millis().min(u64::MAX as u128) as u64
}

#[derive(Clone)]
pub(crate) struct Diagnostics {
    state: Option<Arc<Mutex<State>>>,
    next_id: Arc<AtomicU64>,
}

impl Default for Diagnostics {
    fn default() -> Self {
        Self::new(true)
    }
}

#[derive(Default)]
struct State {
    epoch: Option<Instant>,
    started_requests: u64,
    started_executions: u64,
    active_requests: u64,
    active_executions: u64,
    queued_executions: u64,
    running_executions: u64,
    zero_consumer_executions: u64,
    shared_waiters: u64,
    peak_requests: u64,
    peak_executions: u64,
    omitted_active_details: u64,
    request_outcomes: BTreeMap<&'static str, u64>,
    execution_outcomes: BTreeMap<&'static str, u64>,
    request_reuse: BTreeMap<&'static str, u64>,
    tools: BTreeMap<&'static str, ToolStats>,
    gauges: BTreeMap<&'static str, u64>,
    requests: HashMap<u64, Value>,
    executions: HashMap<u64, Value>,
    recent_requests: VecDeque<(Value, usize)>,
    recent_executions: VecDeque<(Value, usize)>,
    retained_bytes: usize,
}

#[derive(Default, serde::Serialize)]
struct ToolStats {
    requests: u64,
    executions: u64,
    request_outcomes: BTreeMap<&'static str, u64>,
    execution_outcomes: BTreeMap<&'static str, u64>,
    request_wall_ms: [u64; 10],
    execution_wall_ms: [u64; 10],
    queue_ms: [u64; 10],
}

impl ToolStats {
    fn observe(histogram: &mut [u64; 10], ms: u64) {
        if let Some(index) = HISTOGRAM_MS.iter().position(|bound| ms <= *bound) {
            histogram[index] += 1;
        }
    }
}

#[derive(Clone, Copy)]
enum Kind {
    Request,
    Execution,
}

struct Lifecycle {
    finished: bool,
    phase: &'static str,
    running_at: Option<Instant>,
    consumers: usize,
    peak_consumers: usize,
    zero_since: Option<Instant>,
    zero_duration_ms: u64,
    reuse: &'static str,
    execution: Weak<ExecutionTicket>,
    persistence_committed: bool,
}

struct Ticket {
    diagnostics: Diagnostics,
    id: u64,
    kind: Kind,
    tool: &'static str,
    start: Instant,
    lifecycle: Mutex<Lifecycle>,
}

pub(crate) struct RequestTicket(Ticket);
pub(crate) struct ExecutionTicket(Ticket);

impl Diagnostics {
    pub(crate) fn new(enabled: bool) -> Self {
        Self {
            state: enabled.then(|| Arc::new(Mutex::new(State::default()))),
            next_id: Arc::new(AtomicU64::new(1)),
        }
    }

    pub(crate) fn begin_request(&self, tool: &str, project: Option<&str>) -> RequestTicket {
        RequestTicket(self.begin(Kind::Request, tool, project))
    }

    pub(crate) fn begin_execution(&self, tool: &str, project: Option<&str>) -> ExecutionTicket {
        ExecutionTicket(self.begin(Kind::Execution, tool, project))
    }

    fn begin(&self, kind: Kind, tool: &str, project: Option<&str>) -> Ticket {
        let start = Instant::now();
        let tool = tool_name(tool);
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        if let Some(state) = &self.state {
            let mut state = state.lock();
            let epoch = *state.epoch.get_or_insert(start);
            let record = json!({
                "id": id, "tool": tool, "project": project.map(bounded_project),
                "started_after_ms": millis(epoch, start),
                "project_truncated": project.is_some_and(|p| p.len() > PROJECT_LIMIT),
                "phase": "queued", "work_class": "unknown", "execution_id": null, "reuse": "none",
                "persistence_committed": false
            });
            match kind {
                Kind::Request => {
                    state.started_requests += 1;
                    state.active_requests += 1;
                    state.peak_requests = state.peak_requests.max(state.active_requests);
                    state.tools.entry(tool).or_default().requests += 1;
                    if state.requests.len() < DETAIL_LIMIT {
                        state.requests.insert(id, record);
                    } else {
                        state.omitted_active_details += 1;
                    }
                }
                Kind::Execution => {
                    state.started_executions += 1;
                    state.active_executions += 1;
                    state.queued_executions += 1;
                    state.peak_executions = state.peak_executions.max(state.active_executions);
                    state.tools.entry(tool).or_default().executions += 1;
                    if state.executions.len() < DETAIL_LIMIT {
                        state.executions.insert(id, record);
                    } else {
                        state.omitted_active_details += 1;
                    }
                }
            }
        }
        Ticket {
            diagnostics: self.clone(),
            id,
            kind,
            tool,
            start,
            lifecycle: Mutex::new(Lifecycle {
                finished: false,
                phase: "queued",
                running_at: None,
                consumers: 1,
                peak_consumers: 1,
                zero_since: None,
                zero_duration_ms: 0,
                reuse: "none",
                execution: Weak::new(),
                persistence_committed: false,
            }),
        }
    }

    pub(crate) fn set_gauge(&self, name: &str, value: u64) {
        let Some(state) = &self.state else {
            return;
        };
        let name = match name {
            "cache_entries" => "cache_entries",
            "cache_bytes" => "cache_bytes",
            "cache_observers" => "cache_observers",
            "single_flights" => "single_flights",
            "retired_flights" => "retired_flights",
            "project_gates" => "project_gates",
            _ => return,
        };
        state.lock().gauges.insert(name, value);
    }

    pub(crate) fn snapshot(&self, recent: usize) -> Value {
        let Some(state) = &self.state else {
            return json!({"enabled": false});
        };
        let state = state.lock();
        let recent = recent.min(DETAIL_LIMIT);
        let mut requests: Vec<_> = state.requests.values().cloned().collect();
        let mut executions: Vec<_> = state.executions.values().cloned().collect();
        let now_ms = state.epoch.map_or(0, |epoch| millis(epoch, Instant::now()));
        for record in requests.iter_mut().chain(executions.iter_mut()) {
            record["wall_ms"] =
                json!(now_ms.saturating_sub(record["started_after_ms"].as_u64().unwrap_or(0)));
        }
        requests.sort_by_key(|v| v["id"].as_u64());
        executions.sort_by_key(|v| v["id"].as_u64());
        json!({
            "enabled": true,
            "counters": {
                "requests_started": state.started_requests,
                "executions_started": state.started_executions,
                "request_outcomes": state.request_outcomes,
                "execution_outcomes": state.execution_outcomes,
                "request_reuse": state.request_reuse,
                "active_detail_omissions": state.omitted_active_details,
            },
            "gauges": {
                "active_requests": state.active_requests,
                "active_executions": state.active_executions,
                "queued_executions": state.queued_executions,
                "running_executions": state.running_executions,
                "zero_consumer_executions": state.zero_consumer_executions,
                "shared_waiters": state.shared_waiters,
                "peak_requests": state.peak_requests,
                "peak_executions": state.peak_executions,
                "cache": state.gauges,
            },
            "histogram_upper_bounds_ms": HISTOGRAM_MS,
            "histogram_semantics": "non-cumulative bucket counts; wall time, not CPU time",
            "tools": state.tools,
            "active_requests": requests,
            "active_executions": executions,
            "recent_requests": state.recent_requests.iter().rev().take(recent).map(|(v, _)| v).collect::<Vec<_>>(),
            "recent_executions": state.recent_executions.iter().rev().take(recent).map(|(v, _)| v).collect::<Vec<_>>(),
            "retention": {
                "completed_limit_per_kind": DETAIL_LIMIT,
                "active_detail_limit_per_kind": DETAIL_LIMIT,
                "completed_payload_byte_limit": BYTE_LIMIT,
                "completed_payload_bytes": state.retained_bytes,
                "request_records": state.recent_requests.len(),
                "execution_records": state.recent_executions.len(),
            }
        })
    }
}

impl Ticket {
    fn set_class(&self, class: &str) {
        let class = match class {
            "interactive" => "interactive",
            "analysis" => "analysis",
            "native" => "native",
            "control" => "control",
            _ => "unknown",
        };
        self.update(|record| record["work_class"] = json!(class));
    }

    fn update(&self, update: impl FnOnce(&mut Value)) {
        let Some(state) = &self.diagnostics.state else {
            return;
        };
        let lifecycle = self.lifecycle.lock();
        if lifecycle.finished {
            return;
        }
        let mut state = state.lock();
        let records = match self.kind {
            Kind::Request => &mut state.requests,
            Kind::Execution => &mut state.executions,
        };
        if let Some(record) = records.get_mut(&self.id) {
            update(record);
        }
    }

    fn set_phase(&self, phase: &str) {
        let phase = phase_name(phase);
        let mut live = self.lifecycle.lock();
        if live.finished || live.phase == phase {
            return;
        }
        let Some(state) = &self.diagnostics.state else {
            live.phase = phase;
            return;
        };
        let mut state = state.lock();
        match self.kind {
            Kind::Execution => {
                if live.phase == "queued" {
                    state.queued_executions -= 1;
                } else {
                    state.running_executions -= 1;
                }
                if phase == "queued" {
                    state.queued_executions += 1;
                } else {
                    state.running_executions += 1;
                    live.running_at.get_or_insert_with(Instant::now);
                }
                if let Some(record) = state.executions.get_mut(&self.id) {
                    record["phase"] = json!(phase);
                }
            }
            Kind::Request => {
                if live.phase == "shared_wait" {
                    state.shared_waiters -= 1;
                }
                if phase == "shared_wait" {
                    state.shared_waiters += 1;
                }
                if let Some(record) = state.requests.get_mut(&self.id) {
                    record["phase"] = json!(phase);
                }
            }
        }
        live.phase = phase;
    }

    fn finish(&self, outcome: &str) {
        let mut live = self.lifecycle.lock();
        if live.finished {
            return;
        }
        live.finished = true;
        let Some(state) = &self.diagnostics.state else {
            return;
        };
        let end = Instant::now();
        let wall_ms = millis(self.start, end);
        let outcome = outcome_name(outcome);
        let mut state = state.lock();
        let record = match self.kind {
            Kind::Request => {
                state.active_requests -= 1;
                if live.phase == "shared_wait" {
                    state.shared_waiters -= 1;
                }
                *state.request_outcomes.entry(outcome).or_default() += 1;
                *state.request_reuse.entry(live.reuse).or_default() += 1;
                *state
                    .tools
                    .entry(self.tool)
                    .or_default()
                    .request_outcomes
                    .entry(outcome)
                    .or_default() += 1;
                ToolStats::observe(
                    &mut state.tools.entry(self.tool).or_default().request_wall_ms,
                    wall_ms,
                );
                state.requests.remove(&self.id)
            }
            Kind::Execution => {
                state.active_executions -= 1;
                if live.phase == "queued" {
                    state.queued_executions -= 1;
                } else {
                    state.running_executions -= 1;
                }
                if live.consumers == 0 {
                    state.zero_consumer_executions -= 1;
                }
                *state.execution_outcomes.entry(outcome).or_default() += 1;
                let stats = state.tools.entry(self.tool).or_default();
                *stats.execution_outcomes.entry(outcome).or_default() += 1;
                if let Some(at) = live.running_at {
                    ToolStats::observe(&mut stats.execution_wall_ms, millis(at, end));
                }
                ToolStats::observe(
                    &mut stats.queue_ms,
                    millis(self.start, live.running_at.unwrap_or(end)),
                );
                state.executions.remove(&self.id)
            }
        };
        if let Some(mut record) = record {
            record["outcome"] = json!(outcome);
            record["wall_ms"] = json!(wall_ms);
            record["persistence_committed"] = json!(live.persistence_committed);
            if matches!(self.kind, Kind::Execution) {
                record["queue_ms"] = json!(millis(self.start, live.running_at.unwrap_or(end)));
                record["execution_wall_ms"] = json!(live.running_at.map(|at| millis(at, end)));
                record["peak_consumers"] = json!(live.peak_consumers);
                record["zero_consumer_ms"] =
                    json!(live.zero_duration_ms + live.zero_since.map_or(0, |at| millis(at, end)));
            }
            state.retain(self.kind, record);
        }
    }
}

impl State {
    fn retain(&mut self, kind: Kind, record: Value) {
        let bytes = record.to_string().len();
        let ring = match kind {
            Kind::Request => &mut self.recent_requests,
            Kind::Execution => &mut self.recent_executions,
        };
        if ring.len() == DETAIL_LIMIT
            && let Some((_, removed)) = ring.pop_front()
        {
            self.retained_bytes -= removed;
        }
        ring.push_back((record, bytes));
        self.retained_bytes += bytes;
        while self.retained_bytes > BYTE_LIMIT {
            let requests_first =
                match (self.recent_requests.front(), self.recent_executions.front()) {
                    (Some((a, _)), Some((b, _))) => a["id"].as_u64() < b["id"].as_u64(),
                    (Some(_), None) => true,
                    _ => false,
                };
            let removed = if requests_first {
                self.recent_requests.pop_front()
            } else {
                self.recent_executions.pop_front()
            };
            if let Some((_, bytes)) = removed {
                self.retained_bytes -= bytes;
            } else {
                break;
            }
        }
    }
}

impl RequestTicket {
    pub(crate) fn set_class(&self, class: &str) {
        self.0.set_class(class);
    }
    pub(crate) fn id(&self) -> u64 {
        self.0.id
    }
    pub(crate) fn set_project(&self, project: &str) {
        self.0.update(|record| {
            record["project"] = json!(bounded_project(project));
            record["project_truncated"] = json!(project.len() > PROJECT_LIMIT);
        });
    }
    pub(crate) fn link_execution_ticket(&self, execution: &Arc<ExecutionTicket>) {
        let mut live = self.0.lifecycle.lock();
        if live.finished {
            return;
        }
        live.execution = Arc::downgrade(execution);
        let Some(state) = &self.0.diagnostics.state else {
            return;
        };
        let mut state = state.lock();
        if let Some(record) = state.requests.get_mut(&self.0.id) {
            record["execution_id"] = json!(execution.id());
        }
    }
    pub(crate) fn work_state(&self) -> (&'static str, bool) {
        let live = self.0.lifecycle.lock();
        if let Some(execution) = live.execution.upgrade() {
            let work = execution.0.lifecycle.lock();
            (work.phase, !work.finished)
        } else {
            (live.phase, false)
        }
    }
    pub(crate) fn persistence_committed(&self) -> bool {
        self.0.lifecycle.lock().persistence_committed
    }
    pub(crate) fn mark_persisted(&self) {
        let mut live = self.0.lifecycle.lock();
        live.persistence_committed = true;
        let Some(state) = &self.0.diagnostics.state else {
            return;
        };
        let mut state = state.lock();
        if let Some(record) = state.requests.get_mut(&self.0.id) {
            record["persistence_committed"] = json!(true);
        } else if let Some((record, bytes)) = state
            .recent_requests
            .iter_mut()
            .find(|(record, _)| record["id"] == self.0.id)
        {
            record["persistence_committed"] = json!(true);
            let previous = *bytes;
            *bytes = record.to_string().len();
            let removed = previous - *bytes;
            state.retained_bytes -= removed;
        }
    }
    pub(crate) fn set_reuse(&self, reuse: &str) {
        let Some(state) = &self.0.diagnostics.state else {
            return;
        };
        let reuse = match reuse {
            "hit" => "hit",
            "miss" => "miss",
            "shared" => "shared",
            _ => "none",
        };
        let mut live = self.0.lifecycle.lock();
        if live.finished {
            return;
        }
        live.reuse = reuse;
        let mut state = state.lock();
        if let Some(record) = state.requests.get_mut(&self.0.id) {
            record["reuse"] = json!(reuse);
        }
    }
    pub(crate) fn set_phase(&self, phase: &str) {
        self.0.set_phase(phase);
    }
    pub(crate) fn finish(&self, outcome: &str) {
        self.0.finish(outcome);
    }
}

impl ExecutionTicket {
    pub(crate) fn set_class(&self, class: &str) {
        self.0.set_class(class);
    }
    pub(crate) fn id(&self) -> u64 {
        self.0.id
    }
    pub(crate) fn set_phase(&self, phase: &str) {
        self.0.set_phase(phase);
    }
    pub(crate) fn set_consumers(&self, consumers: usize) {
        let mut live = self.0.lifecycle.lock();
        if live.finished {
            return;
        }
        let Some(state) = &self.0.diagnostics.state else {
            live.consumers = consumers;
            return;
        };
        let mut state = state.lock();
        if consumers == 0 && live.consumers != 0 {
            state.zero_consumer_executions += 1;
            live.zero_since = Some(Instant::now());
        } else if consumers != 0 && live.consumers == 0 {
            state.zero_consumer_executions -= 1;
            if let Some(at) = live.zero_since.take() {
                live.zero_duration_ms += millis(at, Instant::now());
            }
        }
        live.consumers = consumers;
        live.peak_consumers = live.peak_consumers.max(consumers);
        if let Some(record) = state.executions.get_mut(&self.0.id) {
            record["consumers"] = json!(consumers);
            record["work_continuing_without_consumers"] = json!(consumers == 0);
        }
    }
    pub(crate) fn finish(&self, outcome: &str) {
        self.0.finish(outcome);
    }
}

impl Drop for Ticket {
    fn drop(&mut self) {
        self.finish(if std::thread::panicking() {
            "panic"
        } else {
            "abandoned"
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_diagnostics_preserve_physical_state_and_persistence() {
        let diagnostics = Diagnostics::new(false);
        let request = diagnostics.begin_request("session_start", Some("/project"));
        let execution = Arc::new(diagnostics.begin_execution("overview_ranking", Some("/project")));
        request.link_execution_ticket(&execution);
        request.set_phase("shared_wait");
        execution.set_phase("native");
        execution.set_consumers(0);
        request.finish("cancelled");
        assert_eq!(request.work_state(), ("native", true));
        request.mark_persisted();
        assert!(request.persistence_committed());
        execution.finish("success");
        assert_eq!(request.work_state(), ("native", false));
        execution.set_phase("running");
        execution.finish("error");
        assert_eq!(request.work_state(), ("native", false));
        assert_eq!(diagnostics.snapshot(256), json!({"enabled": false}));
    }

    #[test]
    fn disabled_diagnostics_allocate_no_registry_or_retained_details() {
        let diagnostics = Diagnostics::new(false);
        for _ in 0..300 {
            let request = diagnostics.begin_request("search", Some("/project"));
            request.set_project("/another-project");
            request.set_class("native");
            request.set_reuse("hit");
            request.mark_persisted();
            let execution = Arc::new(diagnostics.begin_execution("search", Some("/project")));
            execution.set_class("native");
            execution.set_phase("running");
            execution.set_consumers(0);
            execution.set_consumers(1);
            request.link_execution_ticket(&execution);
            diagnostics.set_gauge("cache_entries", 12);
            drop(execution);
            assert_eq!(request.work_state(), ("queued", false));
        }
        assert!(diagnostics.state.is_none());
        assert_eq!(diagnostics.snapshot(usize::MAX), json!({"enabled": false}));
    }

    #[test]
    fn disabled_request_and_execution_ids_remain_unique_across_threads() {
        let diagnostics = Diagnostics::new(false);
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let diagnostics = diagnostics.clone();
                std::thread::spawn(move || {
                    (0..100)
                        .flat_map(|_| {
                            let request = diagnostics.begin_request("search", None);
                            let execution = diagnostics.begin_execution("search", None);
                            [request.id(), execution.id()]
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        let ids: std::collections::BTreeSet<_> = threads
            .into_iter()
            .flat_map(|thread| thread.join().unwrap())
            .collect();
        assert_eq!(ids.len(), 1600);
        assert_eq!(ids.first(), Some(&1));
        assert_eq!(ids.last(), Some(&1600));
    }

    #[test]
    fn physical_state_survives_active_detail_omission_and_request_completion() {
        let diagnostics = Diagnostics::default();
        let requests: Vec<_> = (0..=DETAIL_LIMIT)
            .map(|_| diagnostics.begin_request("search", None))
            .collect();
        let request = requests.last().unwrap();
        let execution = Arc::new(diagnostics.begin_execution("search", None));
        request.link_execution_ticket(&execution);
        execution.set_phase("native");
        request.finish("cancelled");
        assert_eq!(request.work_state(), ("native", true));
        execution.finish("success");
        assert_eq!(request.work_state(), ("native", false));
        assert_eq!(
            diagnostics.snapshot(0)["counters"]["active_detail_omissions"],
            1
        );
    }

    #[test]
    fn request_link_does_not_keep_completed_physical_ticket_alive() {
        let diagnostics = Diagnostics::default();
        let request = diagnostics.begin_request("search", None);
        let execution = Arc::new(diagnostics.begin_execution("search", None));
        request.link_execution_ticket(&execution);
        drop(execution);
        assert_eq!(request.work_state(), ("queued", false));
        assert_eq!(diagnostics.snapshot(0)["gauges"]["active_executions"], 0);
    }

    #[test]
    fn committed_persistence_remains_visible_after_cancellation() {
        let diagnostics = Diagnostics::default();
        let request = diagnostics.begin_request("session_start", None);
        request.mark_persisted();
        request.finish("cancelled");
        assert!(request.persistence_committed());
        assert_eq!(
            diagnostics.snapshot(1)["recent_requests"][0]["persistence_committed"],
            true
        );
    }

    #[test]
    fn late_persistence_updates_completed_record_and_retention_bytes() {
        let diagnostics = Diagnostics::default();
        let request = diagnostics.begin_request("session_start", None);
        request.finish("cancelled");
        let previous = diagnostics.snapshot(1)["retention"]["completed_payload_bytes"]
            .as_u64()
            .unwrap();
        request.mark_persisted();
        request.mark_persisted();
        let snapshot = diagnostics.snapshot(1);
        assert!(request.persistence_committed());
        assert_eq!(
            snapshot["recent_requests"][0]["persistence_committed"],
            true
        );
        assert_eq!(
            snapshot["retention"]["completed_payload_bytes"],
            previous - 1
        );
    }

    #[test]
    fn queued_cancellation_does_not_count_as_execution_latency() {
        let diagnostics = Diagnostics::default();
        diagnostics
            .begin_execution("search", None)
            .finish("cancelled");
        let snapshot = diagnostics.snapshot(1);
        assert_eq!(
            snapshot["tools"]["search"]["execution_wall_ms"],
            json!(vec![0; 10])
        );
        assert_eq!(
            snapshot["recent_executions"][0]["execution_wall_ms"],
            Value::Null
        );
        assert_eq!(
            snapshot["tools"]["search"]["execution_outcomes"]["cancelled"],
            1
        );
    }

    #[test]
    fn work_class_is_distinct_from_execution_phase() {
        let diagnostics = Diagnostics::default();
        let request = diagnostics.begin_request("search", None);
        let execution = diagnostics.begin_execution("search", None);
        request.set_class("native");
        execution.set_class("native");
        let snapshot = diagnostics.snapshot(0);
        assert_eq!(snapshot["active_requests"][0]["work_class"], "native");
        assert_eq!(snapshot["active_executions"][0]["work_class"], "native");
        assert_eq!(snapshot["active_executions"][0]["phase"], "queued");
    }

    #[test]
    fn database_contention_has_a_distinct_outcome_counter() {
        let diagnostics = Diagnostics::default();
        diagnostics
            .begin_request("search", None)
            .finish("database-busy");
        let snapshot = diagnostics.snapshot(1);
        assert_eq!(snapshot["counters"]["request_outcomes"]["database-busy"], 1);
        assert_eq!(snapshot["recent_requests"][0]["outcome"], "database-busy");
    }

    #[test]
    fn logical_cancellation_keeps_physical_execution_visible() {
        let diagnostics = Diagnostics::default();
        let request = diagnostics.begin_request("project_overview", Some("/project"));
        let execution = Arc::new(diagnostics.begin_execution("overview_ranking", Some("/project")));
        request.link_execution_ticket(&execution);
        execution.set_phase("running");
        request.finish("cancelled");
        execution.set_consumers(0);
        assert_eq!(request.work_state(), ("running", true));
        let snapshot = diagnostics.snapshot(256);
        assert_eq!(snapshot["gauges"]["active_requests"], 0);
        assert_eq!(snapshot["gauges"]["running_executions"], 1);
        assert_eq!(snapshot["gauges"]["zero_consumer_executions"], 1);
        assert_eq!(
            snapshot["recent_requests"][0]["execution_id"],
            execution.id()
        );
        assert_eq!(
            snapshot["active_executions"][0]["work_continuing_without_consumers"],
            true
        );
        execution.finish("cancelled");
        assert_eq!(request.work_state(), ("running", false));
        drop(request);
        drop(execution);
        let snapshot = diagnostics.snapshot(256);
        assert_eq!(snapshot["gauges"]["active_executions"], 0);
        assert_eq!(snapshot["gauges"]["zero_consumer_executions"], 0);
        assert_eq!(snapshot["counters"]["request_outcomes"]["cancelled"], 1);
        assert_eq!(snapshot["counters"]["execution_outcomes"]["cancelled"], 1);
    }

    #[test]
    fn drop_balances_queued_and_shared_waiter_counters() {
        let diagnostics = Diagnostics::default();
        let request = diagnostics.begin_request("search", None);
        request.set_phase("shared_wait");
        let execution = diagnostics.begin_execution("search", None);
        drop(request);
        drop(execution);
        let snapshot = diagnostics.snapshot(10);
        assert_eq!(snapshot["gauges"]["queued_executions"], 0);
        assert_eq!(snapshot["gauges"]["shared_waiters"], 0);
        assert_eq!(snapshot["counters"]["request_outcomes"]["abandoned"], 1);
        assert_eq!(snapshot["counters"]["execution_outcomes"]["abandoned"], 1);
    }

    #[test]
    fn active_detail_cap_preserves_exact_aggregate_accounting() {
        let diagnostics = Diagnostics::default();
        let requests: Vec<_> = (0..300)
            .map(|_| diagnostics.begin_request("search", None))
            .collect();
        for request in &requests {
            request.set_reuse("hit");
        }
        let snapshot = diagnostics.snapshot(0);
        assert_eq!(snapshot["gauges"]["active_requests"], 300);
        assert_eq!(
            snapshot["active_requests"].as_array().unwrap().len(),
            DETAIL_LIMIT
        );
        assert_eq!(snapshot["counters"]["active_detail_omissions"], 44);
        drop(requests);
        let snapshot = diagnostics.snapshot(0);
        assert_eq!(snapshot["gauges"]["active_requests"], 0);
        assert_eq!(snapshot["counters"]["request_outcomes"]["abandoned"], 300);
        assert_eq!(snapshot["counters"]["request_reuse"]["hit"], 300);
    }

    #[test]
    fn escaped_paths_cannot_exceed_completed_payload_budget() {
        let diagnostics = Diagnostics::default();
        let project = "\u{0001}".repeat(PROJECT_LIMIT);
        for _ in 0..DETAIL_LIMIT {
            diagnostics
                .begin_request("search", Some(&project))
                .finish("success");
            diagnostics
                .begin_execution("search", Some(&project))
                .finish("success");
        }
        let snapshot = diagnostics.snapshot(usize::MAX);
        assert!(
            snapshot["retention"]["completed_payload_bytes"]
                .as_u64()
                .unwrap()
                <= BYTE_LIMIT as u64
        );
        assert!(
            snapshot["retention"]["request_records"].as_u64().unwrap()
                + snapshot["retention"]["execution_records"].as_u64().unwrap()
                < (DETAIL_LIMIT * 2) as u64
        );
    }

    #[test]
    fn retention_and_labels_are_bounded_under_repeated_untrusted_input() {
        let diagnostics = Diagnostics::default();
        let sentinel = format!("SECRET-{}", std::process::id());
        let project = "/é".repeat(PROJECT_LIMIT);
        for _ in 0..3000 {
            let request = diagnostics.begin_request(&sentinel, Some(&project));
            request.set_reuse(&sentinel);
            request.set_phase(&sentinel);
            request.finish(&sentinel);
            let execution = diagnostics.begin_execution(&sentinel, Some(&project));
            execution.finish(&sentinel);
            diagnostics.set_gauge(&sentinel, 10);
        }
        let snapshot = diagnostics.snapshot(usize::MAX);
        assert!(!snapshot.to_string().contains(&sentinel));
        assert_eq!(snapshot["tools"].as_object().unwrap().len(), 1);
        assert_eq!(
            snapshot["recent_requests"].as_array().unwrap().len(),
            DETAIL_LIMIT
        );
        assert_eq!(
            snapshot["recent_executions"].as_array().unwrap().len(),
            DETAIL_LIMIT
        );
        assert!(
            snapshot["retention"]["completed_payload_bytes"]
                .as_u64()
                .unwrap()
                <= BYTE_LIMIT as u64
        );
        assert_eq!(snapshot["recent_requests"][0]["project_truncated"], true);
        assert!(
            snapshot["recent_requests"][0]["project"]
                .as_str()
                .unwrap()
                .len()
                <= PROJECT_LIMIT
        );
    }

    #[test]
    fn tickets_can_move_to_blocking_threads() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<RequestTicket>();
        assert_send_sync::<ExecutionTicket>();
        let diagnostics = Diagnostics::default();
        let execution = diagnostics.begin_execution("search", None);
        std::thread::spawn(move || {
            execution.set_phase("native");
            execution.finish("success");
        })
        .join()
        .unwrap();
        let snapshot = diagnostics.snapshot(1);
        assert_eq!(snapshot["gauges"]["active_executions"], 0);
        assert_eq!(snapshot["counters"]["execution_outcomes"]["success"], 1);
        assert_eq!(
            snapshot["tools"]["search"]["execution_wall_ms"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_u64().unwrap())
                .sum::<u64>(),
            1
        );
    }
}
