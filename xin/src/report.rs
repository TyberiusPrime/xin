//! B9 output discipline: every command's primary output is JSON; the human
//! rendering is a formatted subset of the same data, never extra facts.

use serde::Serialize;
use xin_driver::driver::QUERY_ONLY_EXIT;
use xin_resolver::failure::{FailureDetail, FailureId, FailureKind, Origin};
use xin_resolver::input::Dag;
use xin_resolver::resolver::{NodeStatus, Outcome};

#[derive(Serialize, Clone)]
pub struct ChainLink {
    pub origin: String,
    pub kind: String,
}

#[derive(Serialize, Clone)]
pub struct FailureReport {
    pub kind: String,
    pub origin: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// blame chain toward the root cause (§7); an `Upstream` link's origin
    /// is the blamed upstream node
    pub chain: Vec<ChainLink>,
}

#[derive(Serialize, Clone)]
pub struct NodeReport {
    pub name: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub store: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure: Option<FailureReport>,
}

pub fn origin_name(dag: &Dag, origin: Origin) -> String {
    match origin {
        Origin::Node(n) => dag.nodes[n.idx()].name.as_str().to_owned(),
        Origin::Output(oh) => format!("output {oh}"),
    }
}

fn detail_string(detail: &FailureDetail) -> Option<String> {
    match detail {
        FailureDetail::None => None,
        FailureDetail::Text(t) => Some(t.clone()),
        FailureDetail::BuildLog(log) => {
            let stderr = String::from_utf8_lossy(&log.stderr);
            let tail: Vec<&str> = stderr.lines().rev().take(5).collect();
            let tail: Vec<&str> = tail.into_iter().rev().collect();
            Some(format!("exit {}: {}", log.return_code, tail.join(" | ")))
        }
        FailureDetail::ConflictingAnswers(answers) => Some(format!(
            "stores disagree: {}",
            answers
                .iter()
                .map(|(s, oh)| format!("{s} says {oh}"))
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

pub fn failure_report(out: &Outcome, dag: &Dag, fid: FailureId) -> FailureReport {
    let rec = &out.failures[fid.idx()];
    let chain = out
        .chain(fid)
        .into_iter()
        .map(|f| {
            let r = &out.failures[f.idx()];
            ChainLink {
                origin: origin_name(dag, r.origin),
                kind: format!("{:?}", r.kind),
            }
        })
        .collect();
    FailureReport {
        kind: format!("{:?}", rec.kind),
        origin: origin_name(dag, rec.origin),
        detail: detail_string(&rec.detail),
        chain,
    }
}

/// True when this failure is only the driver refusing to build/download in
/// query-only mode — `xin status` renders those as work, not as errors.
fn is_query_refusal(out: &Outcome, fid: FailureId) -> bool {
    let rec = &out.failures[fid.idx()];
    match (&rec.kind, &rec.detail) {
        (FailureKind::Build, FailureDetail::BuildLog(log)) => log.return_code == QUERY_ONLY_EXIT,
        (FailureKind::Download, FailureDetail::Text(t)) => t.contains("status query"),
        _ => false,
    }
}

/// One report per node. `status_mode` reinterprets refused work: a
/// query-refusal becomes "needs-build", upstreams of one become "blocked".
pub fn node_reports(out: &Outcome, dag: &Dag, status_mode: bool) -> Vec<NodeReport> {
    dag.nodes
        .iter()
        .enumerate()
        .map(|(i, node)| {
            let name = node.name.as_str().to_owned();
            match &out.statuses[i] {
                NodeStatus::Realized { output, store } => NodeReport {
                    name,
                    status: if status_mode { "present" } else { "realized" }.into(),
                    output: Some(output.to_string()),
                    store: Some(store.to_string()),
                    failure: None,
                },
                NodeStatus::Named { output } => NodeReport {
                    name,
                    status: "named".into(),
                    output: Some(output.to_string()),
                    store: None,
                    failure: None,
                },
                NodeStatus::Incomplete => NodeReport {
                    name,
                    status: "incomplete".into(),
                    output: None,
                    store: None,
                    failure: None,
                },
                NodeStatus::Failed { failure } => {
                    let rec = &out.failures[failure.idx()];
                    let status = if !status_mode {
                        "failed"
                    } else if is_query_refusal(out, *failure) {
                        "needs-build"
                    } else if rec.kind == FailureKind::Upstream {
                        let root = *out.chain(*failure).last().unwrap();
                        if is_query_refusal(out, root) {
                            "blocked"
                        } else {
                            "failed"
                        }
                    } else {
                        "failed"
                    };
                    let failure = (status == "failed" || status == "blocked")
                        .then(|| failure_report(out, dag, *failure));
                    NodeReport {
                        name,
                        status: status.into(),
                        output: None,
                        store: None,
                        failure,
                    }
                }
            }
        })
        .collect()
}

/// The human line block for a node list — used by build and status alike.
pub fn render_nodes(nodes: &[NodeReport]) -> String {
    let width = nodes.iter().map(|n| n.name.len()).max().unwrap_or(0);
    let mut s = String::new();
    for n in nodes {
        let mark = match n.status.as_str() {
            "realized" | "present" => "✓",
            "named" => "·",
            "needs-build" | "blocked" => "○",
            "incomplete" => "…",
            _ => "✗",
        };
        s.push_str(&format!("{mark} {:width$}  {}", n.name, n.status));
        if let Some(oh) = &n.output {
            s.push_str(&format!("  {}…", &oh[..16]));
        }
        if let Some(store) = &n.store {
            s.push_str(&format!("  in {store}"));
        }
        s.push('\n');
        if let Some(f) = &n.failure {
            if f.chain.len() > 1 {
                // path of blamed nodes (consecutive duplicates folded — an
                // Upstream link and its target's own record share an origin)
                let mut path: Vec<&str> = Vec::new();
                for link in &f.chain {
                    if path.last() != Some(&link.origin.as_str()) {
                        path.push(&link.origin);
                    }
                }
                let root_kind = &f.chain.last().unwrap().kind;
                s.push_str(&format!("    blame: {} ({root_kind})\n", path.join(" ← ")));
            }
            if let Some(d) = &f.detail {
                s.push_str(&format!("    {d}\n"));
            }
        }
    }
    s
}
