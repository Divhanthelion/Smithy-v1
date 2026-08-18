//! Build a Session the same way the editor does, minus floem.
//!
//! Tool set, Map, MCP, and hooks stay in lockstep with `apps/smithy/src/agent.rs`.
//! If that file grows a tool, this file should too.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use smithy_agent::{
    load_harness, system_prompt, AgentConfig, Harness, Session, SessionConfig, Skill,
};
use smithy_project::{ContextBudget, Project, ProjectRegistry};
use smithy_tools::{Registry, ToolCtx, Workspace};

use crate::hooks::{ShellApprovalHook, WriteReviewHook};

pub struct Booted {
    pub session: Session,
    pub model_label: String,
    pub context_summary: String,
    pub notices: Vec<String>,
    pub auto_approve: Arc<AtomicBool>,
    pub harness: Harness,
}

pub async fn boot(project: &Project, yolo: bool, skill: Option<Skill>) -> Result<Booted, String> {
    let data_dir = ProjectRegistry::default_location()
        .map(|r| r.data_dir().to_path_buf())
        .unwrap_or_else(|_| std::env::temp_dir().join("smithy"));
    let config = AgentConfig::load(&data_dir);
    let provider_choice = config.provider;
    let provider = tokio::task::spawn_blocking(move || config.build_provider())
        .await
        .map_err(|e| format!("provider setup failed: {e}"))?
        .map_err(|e| e.to_string())?;

    let brave_configured = {
        use smithy_agent::config::{secrets, BRAVE_KEY};
        std::env::var("BRAVE_API_KEY")
            .ok()
            .is_some_and(|k| !k.trim().is_empty())
            || secrets::is_stored(BRAVE_KEY)
    };

    let info = provider.probe_model().await.map_err(|e| e.to_string())?;
    provider.preflight().await.map_err(|e| e.to_string())?;

    let (model_label, mut limits) = match &info {
        Some(info) => (info.label(), info.suggested_limits()),
        None => (
            provider.model().to_string(),
            smithy_agent::Limits::default(),
        ),
    };
    limits.max_seconds = skill
        .as_ref()
        .and_then(|s| s.meta.max_seconds)
        .unwrap_or_else(|| provider_choice.turn_seconds());
    let unbounded_search = skill
        .as_ref()
        .and_then(|s| s.meta.tools.as_ref())
        .is_some_and(|t| t.iter().any(|n| n == "web_search"));

    let budget = ContextBudget::for_window(info.as_ref().and_then(|i| i.context_length));
    let graph = ProjectRegistry::default_location().ok().and_then(|reg| {
        let path = reg.callgraph_path(&project.root);
        smithy_project::callgraph::CallGraph::load(&path).ok()
    });
    let context_project = project.clone();
    let extracted = tokio::task::spawn_blocking(move || {
        context_project.context_with_graph(budget, graph.as_ref())
    })
    .await
    .map_err(|e| format!("project scan failed: {e}"))?;
    for warning in &extracted.warnings {
        eprintln!("[project] {warning}");
    }

    let workspace = Workspace::open(&project.root)?;
    let index_root = project.root.clone();
    let symbol_index = tokio::task::spawn_blocking(move || {
        Arc::new(smithy_project::symbols::SymbolIndex::build(&index_root))
    })
    .await
    .map_err(|e| format!("symbol index failed: {e}"))?;

    let mut registry = assemble_registry(
        unbounded_search,
        provider.clone(),
        &project.root,
        brave_configured,
        symbol_index,
    );

    let mcp = smithy_agent::mcp::attach_mcp(&project.root, &smithy_agent::mcp::RmcpConnector).await;
    let mut notices = mcp.notices;
    let already: Vec<String> = registry.names().into_iter().map(str::to_string).collect();
    for tool in mcp.tools {
        if already.iter().any(|n| n == tool.name()) {
            notices.push(format!(
                "MCP tool `{}` skipped: collides with a core tool.",
                tool.name()
            ));
            continue;
        }
        registry.push(tool);
    }
    if let Some(allow) = skill.as_ref().and_then(|s| s.meta.tools.as_ref()) {
        retain_skill_tools(&mut registry, allow, skill.as_ref(), &mut notices);
    }

    let auto_approve = Arc::new(AtomicBool::new(yolo));
    let review_writes = {
        let names = registry.names();
        names.contains(&"write") || names.contains(&"edit")
    };
    let has_bash = registry.names().contains(&"bash");
    if review_writes {
        registry.add_hook(Box::new(WriteReviewHook {
            auto_approve: auto_approve.clone(),
        }));
    }
    if has_bash {
        registry.add_hook(Box::new(ShellApprovalHook {
            auto_approve: auto_approve.clone(),
        }));
    }

    let project_chars = extracted.rendered.len();
    let harness = load_harness(&project.root);
    let mut prompt = system_prompt(
        workspace.root(),
        &registry.names(),
        Some(&extracted.rendered),
    );
    if let Some(skill) = &skill {
        prompt = format!("{prompt}\n\n{}", skill.injection());
    }
    let system_base_chars = prompt.len().saturating_sub(project_chars);
    let ctx = Arc::new(ToolCtx::new(workspace));
    let session_skill = skill.as_ref().map(|s| s.meta.name.clone());
    let mut config = SessionConfig::new(prompt)
        .with_segments(system_base_chars, project_chars)
        .with_skill(session_skill);
    config.limits = limits;

    notices.splice(0..0, harness.notices.iter().cloned());
    let mut harness_line = format!("Harness: {}", harness.source.label());
    if !harness.includes.is_empty() {
        let names: Vec<&str> = harness.includes.iter().map(|i| i.name.as_str()).collect();
        harness_line.push_str(&format!(" + {}", names.join(", ")));
    }
    notices.insert(0, harness_line);

    let context_summary = format!(
        "{} · ~{} tokens",
        extracted
            .layers
            .iter()
            .map(|l| l.label())
            .collect::<Vec<_>>()
            .join(", "),
        extracted.approx_tokens()
    );

    Ok(Booted {
        session: Session::new(provider, Arc::new(registry), ctx, config),
        model_label,
        context_summary,
        notices,
        auto_approve,
        harness,
    })
}

fn assemble_registry(
    unbounded_search: bool,
    provider: Arc<dyn smithy_agent::Provider>,
    project_root: &std::path::Path,
    brave_configured: bool,
    symbol_index: Arc<smithy_project::symbols::SymbolIndex>,
) -> Registry {
    let mut registry = Registry::core();
    registry.push(Box::new(smithy_tools::tools::web_fetch::WebFetch::new()));
    if brave_configured {
        registry.push(Box::new(brave_search(unbounded_search)));
    }
    if !symbol_index.is_empty() {
        registry.push(Box::new(smithy_agent::SymbolLookup::new(symbol_index)));
    }
    registry.push(Box::new(smithy_agent::Explore::new(
        provider,
        project_root,
        if brave_configured {
            vec![Box::new(brave_search(false)) as Box<dyn smithy_tools::Tool>]
        } else {
            Vec::new()
        },
    )));
    registry
}

fn brave_search(research: bool) -> smithy_tools::tools::web_search::WebSearch {
    let search = smithy_tools::tools::web_search::WebSearch::deferred(|| {
        smithy_agent::config::api_key(smithy_agent::config::BRAVE_KEY, "BRAVE_API_KEY")
    });
    if research {
        search.for_research()
    } else {
        search
    }
}

fn is_coding_tool(name: &str) -> bool {
    matches!(
        name,
        "read"
            | "write"
            | "edit"
            | "ls"
            | "glob"
            | "grep"
            | "bash"
            | "todo"
            | "web_fetch"
            | "web_search"
            | "symbol"
            | "explore"
    )
}

fn retain_skill_tools(
    registry: &mut Registry,
    allow: &[String],
    skill: Option<&Skill>,
    notices: &mut Vec<String>,
) {
    let present: Vec<String> = registry.names().into_iter().map(str::to_string).collect();
    if let Some(skill) = skill {
        for asked in allow {
            if !present.iter().any(|n| n == asked) {
                notices.push(format!(
                    "Skill `{}` asked for tool `{asked}` which is not available.",
                    skill.meta.name
                ));
            }
        }
    }
    let mut keep = allow.to_vec();
    for n in &present {
        if !is_coding_tool(n) {
            keep.push(n.clone());
        }
    }
    registry.retain_named(&keep);
}

pub fn save_session(
    project: &Project,
    session: &Session,
    model: &str,
    id: &str,
) -> Result<(), String> {
    let store = match ProjectRegistry::default_location() {
        Ok(reg) => smithy_agent::SessionStore::new(reg.sessions_dir(&project.root))?,
        Err(_) => return Ok(()),
    };
    let mut stored = smithy_agent::persist::StoredSession::from_history_with_reasoning(
        id,
        &project.root,
        model,
        session.history(),
        session.sampling(),
        session.limits(),
        session.reasoning().to_vec(),
        session.skill().map(str::to_string),
    );
    stored = stored.with_tools(session.tools_schema().clone());
    store.save(&stored)
}

pub fn new_session_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!(
        "{secs}-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}
