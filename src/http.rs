//! Streamable HTTP transport wiring.
//!
//! Builds the axum [`Router`](axum::Router) that mounts the MCP server as a
//! Streamable HTTP endpoint. Kept separate from `main` so the router can be
//! constructed and exercised in tests without binding a real listener.

use rmcp::transport::streamable_http_server::StreamableHttpService;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;

use crate::config::ServerConfig;
use crate::server::MailImapServer;

/// Build the axum router serving the MCP server over Streamable HTTP at `path`.
///
/// The session manager constructs a fresh [`MailImapServer`] per session from a
/// clone of `config`; construction is cheap (it just holds configuration and
/// connects to IMAP/SMTP lazily per tool call). `update_notice`, if present, is
/// surfaced to every session exactly as it is over stdio.
pub fn build_router(
    config: ServerConfig,
    update_notice: Option<String>,
    path: &str,
) -> axum::Router {
    let service = StreamableHttpService::new(
        move || Ok(MailImapServer::new(config.clone(), update_notice.clone())),
        LocalSessionManager::default().into(),
        Default::default(),
    );

    axum::Router::new().nest_service(path, service)
}

#[cfg(test)]
mod tests {
    use super::build_router;
    use crate::config::ServerConfig;

    /// Boot the router on an ephemeral port and drive a real MCP `initialize`
    /// handshake over HTTP, proving the Streamable HTTP transport serves the
    /// mail server end to end.
    #[tokio::test]
    async fn http_transport_responds_to_initialize() {
        let router = build_router(ServerConfig::empty_for_test(), None, "/mcp");

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });

        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "test-client", "version": "0.0.0" }
            }
        });

        let client = reqwest::Client::new();
        let resp = client
            .post(format!("http://{addr}/mcp"))
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .json(&body)
            .send()
            .await
            .expect("request to HTTP transport succeeds");

        assert!(
            resp.status().is_success(),
            "expected 2xx from /mcp, got {}",
            resp.status()
        );

        let text = resp.text().await.unwrap();
        assert!(
            text.contains("protocolVersion") && text.contains("serverInfo"),
            "initialize response missing handshake fields: {text}"
        );
    }
}
