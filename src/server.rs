//! MCP Tool Router implementation for `ShadowPTY`.

use rmcp::{
    handler::server::wrapper::Parameters,
    model::CallToolResult,
    schemars::{self, JsonSchema},
    tool, tool_router,
};
use serde::{Deserialize, Serialize};

use crate::pty_manager::PtyManager;

/// Parameters for `tui_start` tool.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct TuiStartParams {
    /// The executable command to spawn (e.g. "top", "htop", "bash").
    pub command: String,
    /// Arguments to pass to the command.
    #[serde(default)]
    pub args: Vec<String>,
    /// Number of terminal rows (default 24).
    pub rows: Option<u16>,
    /// Number of terminal columns (default 80).
    pub cols: Option<u16>,
    /// Optional filesystem path where session will be recorded in asciicast v3 format.
    pub record_path: Option<String>,
}

/// Parameters for `tui_input` tool.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct TuiInputParams {
    /// String containing keystrokes and symbolic tokens (e.g. "<ENTER>", "<ESC>", "<UP>", "<CTRL+C>").
    pub keys: String,
}

/// Parameters for `tui_resize` tool.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct TuiResizeParams {
    /// New number of terminal rows.
    pub rows: u16,
    /// New number of terminal columns.
    pub cols: u16,
}

/// The `ShadowPTY` MCP Server holding the state manager.
#[derive(Clone)]
pub struct ShadowPtyServer {
    manager: PtyManager,
}

impl ShadowPtyServer {
    #[must_use]
    pub const fn new(manager: PtyManager) -> Self {
        Self { manager }
    }
}

#[tool_router(server_handler)]
impl ShadowPtyServer {
    /// Spawns a new process in a native pseudo-terminal (PTY) and initializes the screen buffer.
    #[tool(
        name = "tui_start",
        description = "Spawns a command in a native pseudo-terminal (PTY) and initializes screen tracking. Optionally records to an asciicast v3 file."
    )]
    pub async fn tui_start(
        &self,
        Parameters(params): Parameters<TuiStartParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let rows = params.rows.unwrap_or(24);
        let cols = params.cols.unwrap_or(80);
        let config = crate::pty_manager::PtyConfig::new(&params.command, &params.args, rows, cols)
            .with_record_path(params.record_path.as_deref());

        match self.manager.start_app(&config).await {
            Ok(info) => {
                let pid_str = info
                    .pid
                    .map_or_else(|| "unknown".to_string(), |p| p.to_string());
                let cmd = &info.command;
                let rows = info.rows;
                let cols = info.cols;
                let recording_str = params
                    .record_path
                    .as_ref()
                    .map_or_else(String::new, |path| format!(", recording to '{path}'"));
                let msg = format!(
                    "Started command '{cmd}' in PTY (pid: {pid_str}, rows: {rows}, cols: {cols}{recording_str})"
                );
                Ok(CallToolResult::success(vec![
                    rmcp::model::ContentBlock::text(msg),
                ]))
            }
            Err(e) => Ok(CallToolResult::error(vec![
                rmcp::model::ContentBlock::text(format!("Failed to start process: {e:#}")),
            ])),
        }
    }

    /// Sends raw keystrokes or symbolic tokens (<ENTER>, <UP>, <CTRL+C>, etc.) to the PTY.
    #[tool(
        name = "tui_input",
        description = "Sends keystrokes to the running TUI application. Supports tokens like <ENTER>, <ESC>, <UP>, <DOWN>, <CTRL+C>, etc."
    )]
    pub async fn tui_input(
        &self,
        Parameters(params): Parameters<TuiInputParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        match self.manager.send_input(&params.keys).await {
            Ok(bytes_written) => {
                let msg = format!(
                    "Sent {} bytes to PTY for input {:?}",
                    bytes_written, params.keys
                );
                Ok(CallToolResult::success(vec![
                    rmcp::model::ContentBlock::text(msg),
                ]))
            }
            Err(e) => Ok(CallToolResult::error(vec![
                rmcp::model::ContentBlock::text(format!("Failed to send input: {e:#}")),
            ])),
        }
    }

    /// Resizes the pseudo-terminal window and updates screen parser dimensions.
    #[tool(
        name = "tui_resize",
        description = "Resizes the pseudo-terminal window and screen grid to test TUI responsive layout."
    )]
    pub async fn tui_resize(
        &self,
        Parameters(params): Parameters<TuiResizeParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        match self.manager.resize(params.rows, params.cols).await {
            Ok((rows, cols)) => {
                let msg = format!("Resized terminal to {rows} rows x {cols} cols");
                Ok(CallToolResult::success(vec![
                    rmcp::model::ContentBlock::text(msg),
                ]))
            }
            Err(e) => Ok(CallToolResult::error(vec![
                rmcp::model::ContentBlock::text(format!("Failed to resize terminal: {e:#}")),
            ])),
        }
    }

    /// Reads the current TUI screen state, preserving layout, colors, and text attributes.
    #[tool(
        name = "tui_read",
        description = "Reads the current screen state formatted with semantic tags (<fg:...>, <bg:...>, <bold>, etc.)."
    )]
    pub async fn tui_read(&self) -> Result<CallToolResult, rmcp::ErrorData> {
        match self.manager.read_screen().await {
            Ok(screen_text) => Ok(CallToolResult::success(vec![
                rmcp::model::ContentBlock::text(screen_text),
            ])),
            Err(e) => Ok(CallToolResult::error(vec![
                rmcp::model::ContentBlock::text(format!("Failed to read screen: {e:#}")),
            ])),
        }
    }

    /// Terminates the active pseudo-terminal session, killing the child process and releasing resources.
    #[tool(
        name = "tui_end",
        description = "Terminates the active pseudo-terminal session, killing the running program and releasing resources."
    )]
    pub async fn tui_end(&self) -> Result<CallToolResult, rmcp::ErrorData> {
        match self.manager.stop_app().await {
            Ok(info) => {
                let pid_str = info
                    .pid
                    .map_or_else(|| "unknown".to_string(), |p| p.to_string());
                let cmd = &info.command;
                let msg = format!("Terminated session for command '{cmd}' (pid: {pid_str})");
                Ok(CallToolResult::success(vec![
                    rmcp::model::ContentBlock::text(msg),
                ]))
            }
            Err(e) => Ok(CallToolResult::error(vec![
                rmcp::model::ContentBlock::text(format!("Failed to terminate session: {e:#}")),
            ])),
        }
    }
}
