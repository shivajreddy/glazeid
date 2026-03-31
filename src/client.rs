/// Async GlazeWM IPC client.
///
/// Connects to the GlazeWM WebSocket server, fetches the initial monitor/
/// workspace state, then subscribes to workspace-related events, pushing
/// `BarState` updates through a `tokio::sync::watch` channel so the render
/// loop always sees the latest state without blocking.
use std::collections::HashMap;

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio::sync::watch;
use tokio_tungstenite::{
    connect_async, tungstenite::Message, MaybeTlsStream, WebSocketStream,
};
use crate::ipc::{
    ClientResponseData, MonitorDto, ServerMessage, WmEvent, WorkspaceDto,
};

// ---------------------------------------------------------------------------
// Public state type
// ---------------------------------------------------------------------------

/// Monitor geometry as reported by GlazeWM (logical pixels).
#[derive(Clone, Debug, Default)]
pub struct MonitorGeometry {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub scale_factor: f32,
}

/// Workspace info for a single monitor, ready for the renderer.
#[derive(Clone, Debug, Default)]
pub struct MonitorWorkspaces {
    /// Workspaces on this monitor, in the order returned by GlazeWM.
    pub workspaces: Vec<WorkspaceInfo>,
    /// Monitor geometry from GlazeWM (logical pixels, same coordinate space
    /// GlazeWM uses — reliable across Windows and macOS).
    pub geometry: MonitorGeometry,
}

/// Lean representation of a single workspace.
#[allow(dead_code)]
#[derive(Clone, Debug)]
pub struct WorkspaceInfo {
    /// Numeric or named label shown on the bar.
    pub label: String,
    /// Whether this workspace is currently focused (has keyboard focus).
    pub has_focus: bool,
    /// Whether this workspace is shown on its monitor (visible, regardless of focus).
    pub is_displayed: bool,
}

/// Full bar state shared between the IPC task and the render loop.
///
/// Keyed by GlazeWM monitor `device_name` so each bar window can look up its
/// own monitor's workspaces.
#[derive(Clone, Debug, Default)]
pub struct BarState {
    pub monitors: HashMap<String, MonitorWorkspaces>,
}

impl BarState {
    /// Build a `BarState` from a flat list of `MonitorDto`s returned by
    /// `query monitors`.
    fn from_monitors(monitors: &[MonitorDto]) -> Self {
        let mut state = Self::default();
        for m in monitors {
            let workspaces = m
                .children
                .iter()
                .filter_map(|c| c.as_workspace())
                .map(workspace_info)
                .collect();
            let geometry = MonitorGeometry {
                x: m.x,
                y: m.y,
                width: m.width,
                height: m.height,
                scale_factor: m.scale_factor,
            };
            state.monitors.insert(
                m.device_name.clone(),
                MonitorWorkspaces { workspaces, geometry },
            );
        }
        state
    }

}

fn workspace_info(ws: &WorkspaceDto) -> WorkspaceInfo {
    WorkspaceInfo {
        label: ws
            .display_name
            .clone()
            .unwrap_or_else(|| ws.name.clone()),
        has_focus: ws.has_focus,
        is_displayed: ws.is_displayed,
    }
}

// ---------------------------------------------------------------------------
// IPC connection wrapper
// ---------------------------------------------------------------------------

struct IpcConn {
    stream: WebSocketStream<MaybeTlsStream<TcpStream>>,
}

impl IpcConn {
    async fn connect(port: u16) -> Result<Self> {
        let url = format!("ws://127.0.0.1:{port}");
        let (stream, _) = connect_async(&url)
            .await
            .with_context(|| format!("Failed to connect to GlazeWM at {url}"))?;
        Ok(Self { stream })
    }

    async fn send(&mut self, msg: &str) -> Result<()> {
        self.stream
            .send(Message::Text(msg.into()))
            .await
            .context("Failed to send IPC message")
    }

    /// Wait for the next server message, skipping WebSocket ping/pong frames.
    async fn next(&mut self) -> Result<ServerMessage> {
        loop {
            let raw = self
                .stream
                .next()
                .await
                .context("IPC stream closed")?
                .context("IPC stream error")?;

            match raw {
                Message::Text(text) => {
                    return serde_json::from_str::<ServerMessage>(&text)
                        .with_context(|| {
                            format!("Failed to deserialize server message: {text}")
                        });
                }
                // Ignore control frames; tungstenite handles pong automatically.
                _ => continue,
            }
        }
    }

    /// Block until we receive a `ClientResponse` matching `client_message`.
    async fn client_response(
        &mut self,
        client_message: &str,
    ) -> Result<crate::ipc::ClientResponseMessage> {
        loop {
            match self.next().await? {
                ServerMessage::ClientResponse(r)
                    if r.client_message == client_message =>
                {
                    return Ok(r);
                }
                _ => continue,
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Spawn the IPC background task.
///
/// Returns a `watch::Receiver<BarState>` that always holds the latest workspace
/// state. The task reconnects automatically on connection loss.
pub fn spawn(
    port: u16,
    reconnect_delay_ms: u64,
) -> watch::Receiver<BarState> {
    let (tx, rx) = watch::channel(BarState::default());
    tokio::spawn(ipc_loop(port, reconnect_delay_ms, tx));
    rx
}

async fn ipc_loop(
    port: u16,
    reconnect_delay_ms: u64,
    tx: watch::Sender<BarState>,
) {
    loop {
        match run_session(port, &tx).await {
            Ok(()) => {
                // GlazeWM exited cleanly (ApplicationExiting event or stream closed).
                tracing::info!("GlazeWM IPC session ended; reconnecting in {reconnect_delay_ms}ms.");
            }
            Err(err) => {
                tracing::warn!("IPC error: {err:#}; reconnecting in {reconnect_delay_ms}ms.");
            }
        }

        tokio::time::sleep(tokio::time::Duration::from_millis(reconnect_delay_ms)).await;
    }
}

async fn run_session(
    port: u16,
    tx: &watch::Sender<BarState>,
) -> Result<()> {
    let mut conn = IpcConn::connect(port).await?;
    tracing::info!("Connected to GlazeWM IPC on port {port}.");

    // 1. Fetch initial state.
    let state = fetch_state(&mut conn).await?;
    let _ = tx.send(state);

    // 2. Subscribe to all workspace + focus + monitor events.
    let sub_cmd =
        "sub --events workspace_activated workspace_deactivated workspace_updated \
         focus_changed focused_container_moved \
         monitor_added monitor_removed monitor_updated \
         application_exiting";
    conn.send(sub_cmd).await?;

    // The first response to `sub` is a ClientResponse containing the subscription ID.
    let sub_resp = conn.client_response(sub_cmd).await?;
    if !sub_resp.success {
        anyhow::bail!("Subscription failed: {:?}", sub_resp.error);
    }
    let sub_id = match sub_resp.data {
        Some(ClientResponseData::EventSubscribe(d)) => d.subscription_id,
        _ => anyhow::bail!("Unexpected subscription response data"),
    };
    tracing::debug!(sub_id = %sub_id, "Subscribed to GlazeWM events.");

    // 3. Event loop.
    loop {
        let msg = conn.next().await?;

        match msg {
            ServerMessage::EventSubscription(ev)
                if ev.subscription_id == sub_id =>
            {
                let needs_refetch = match &ev.data {
                    Some(WmEvent::WorkspaceActivated { .. })
                    | Some(WmEvent::WorkspaceDeactivated { .. })
                    | Some(WmEvent::WorkspaceUpdated { .. })
                    | Some(WmEvent::FocusChanged { .. })
                    | Some(WmEvent::FocusedContainerMoved { .. })
                    | Some(WmEvent::MonitorAdded { .. })
                    | Some(WmEvent::MonitorRemoved { .. })
                    | Some(WmEvent::MonitorUpdated { .. }) => true,
                    Some(WmEvent::Other) | None => false,
                };

                if needs_refetch {
                    // Re-fetch full monitor tree to keep state coherent.
                    match fetch_state(&mut conn).await {
                        Ok(state) => {
                            let _ = tx.send(state);
                        }
                        Err(e) => {
                            tracing::warn!("Failed to re-fetch state: {e:#}");
                            return Err(e);
                        }
                    }
                }
            }
            // Ignore unrelated messages (e.g. responses from other commands).
            _ => {}
        }
    }
}

/// Send `query monitors` and parse the response into a `BarState`.
async fn fetch_state(conn: &mut IpcConn) -> Result<BarState> {
    conn.send("query monitors").await?;
    let resp = conn.client_response("query monitors").await?;

    if !resp.success {
        anyhow::bail!("query monitors failed: {:?}", resp.error);
    }

    let monitors = match resp.data {
        Some(ClientResponseData::Monitors(d)) => d.monitors,
        _ => anyhow::bail!("Unexpected data in monitors response"),
    };

    // The top-level list is a list of `ContainerDto::Monitor(...)`.
    let monitor_dtos: Vec<&crate::ipc::MonitorDto> =
        monitors.iter().filter_map(|c| c.as_monitor()).collect();

    Ok(BarState::from_monitors(
        &monitor_dtos.into_iter().cloned().collect::<Vec<_>>(),
    ))
}
