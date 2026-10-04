//! One stderr line describing the connected client.
//!
//! Says who the client is, which protocol it speaks and what it supports, so it shows at a
//! glance whether a client can answer the capture form (elicitation) or run tool calls as
//! tasks, without a protocol trace.

use std::sync::atomic::{AtomicBool, Ordering};

use rmcp::model::{ClientCapabilities, Implementation, ProtocolVersion, TASKS_EXTENSION_ID};

/// What is known about the client when it is first seen.
#[derive(Debug, Clone, Copy, Default)]
pub struct ClientSeen<'a> {
    pub protocol: Option<&'a ProtocolVersion>,
    pub client: Option<&'a Implementation>,
    pub capabilities: Option<&'a ClientCapabilities>,
}

/// Logs the client once per server, on whichever request reveals it first.
#[derive(Debug, Default)]
pub struct ClientLog {
    logged: AtomicBool,
}

impl ClientLog {
    /// Logs `seen` unless a client was already logged.
    pub fn once(&self, seen: ClientSeen<'_>) {
        if !self.logged.swap(true, Ordering::Relaxed) {
            tracing::info!("{}", describe(seen));
        }
    }
}

/// `Client connected: claude-code 2.1.0, protocol 2025-11-25, elicitation: form, tasks: no, …`
#[must_use]
pub fn describe(seen: ClientSeen<'_>) -> String {
    let client = seen.client.map_or_else(
        || "unknown client".to_string(),
        |c| format!("{} {}", c.name, c.version),
    );
    let protocol = seen
        .protocol
        .map_or_else(|| "unknown".to_string(), ToString::to_string);
    let Some(caps) = seen.capabilities else {
        return format!("Client connected: {client}, protocol {protocol}, capabilities unknown");
    };
    let elicitation =
        caps.elicitation
            .as_ref()
            .map_or("no", |e| match (e.form.is_some(), e.url.is_some()) {
                (true, true) => "form and url",
                (_, false) => "form",
                (false, true) => "url only",
            });
    let tasks = if caps.supports_tasks() { "yes" } else { "no" };
    let names = |map: Option<&std::collections::BTreeMap<String, _>>| {
        map.filter(|m| !m.is_empty()).map_or_else(
            || "none".to_string(),
            |m| m.keys().cloned().collect::<Vec<_>>().join(", "),
        )
    };
    format!(
        "Client connected: {client}, protocol {protocol}, elicitation: {elicitation}, tasks ({TASKS_EXTENSION_ID}): {tasks}, extensions: {}, experimental: {}, sampling: {}, roots: {}",
        names(caps.extensions.as_ref()),
        names(caps.experimental.as_ref()),
        yes_no(caps.sampling.is_some()),
        yes_no(caps.roots.is_some()),
    )
}

const fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

#[cfg(test)]
mod tests {
    use rmcp::model::{ElicitationCapability, FormElicitationCapability};

    use super::*;

    #[test]
    fn describes_a_client_with_tasks_and_forms() {
        let caps = ClientCapabilities::builder()
            .enable_tasks()
            .enable_elicitation_with(
                ElicitationCapability::new().with_form(FormElicitationCapability::default()),
            )
            .build();
        let client = Implementation::new("claude-code", "2.1.0");
        let line = describe(ClientSeen {
            protocol: Some(&ProtocolVersion::V_2025_06_18),
            client: Some(&client),
            capabilities: Some(&caps),
        });
        assert!(line.starts_with("Client connected: claude-code 2.1.0, protocol 2025-06-18"));
        assert!(line.contains("elicitation: form,"), "{line}");
        assert!(
            line.contains("tasks (io.modelcontextprotocol/tasks): yes"),
            "{line}"
        );
        assert!(
            line.contains("extensions: io.modelcontextprotocol/tasks"),
            "{line}"
        );
    }

    #[test]
    fn describes_a_bare_client() {
        let caps = ClientCapabilities::default();
        let line = describe(ClientSeen {
            capabilities: Some(&caps),
            ..ClientSeen::default()
        });
        assert!(line.contains("unknown client, protocol unknown"), "{line}");
        assert!(
            line.contains(
                "elicitation: no, tasks (io.modelcontextprotocol/tasks): no, extensions: none"
            ),
            "{line}"
        );
        assert_eq!(
            describe(ClientSeen::default()),
            "Client connected: unknown client, protocol unknown, capabilities unknown"
        );
    }
}
