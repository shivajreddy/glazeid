/// Minimal GlazeWM IPC types.
///
/// These mirror the relevant subset of `wm-common`'s IPC structs so we have no
/// dependency on the upstream crate.
use serde::{Deserialize, Serialize};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

#[allow(dead_code)]
pub const DEFAULT_IPC_PORT: u16 = 6123;

// ---------------------------------------------------------------------------
// Server-to-client envelope
// ---------------------------------------------------------------------------

/// Top-level message emitted by the GlazeWM IPC server.
#[derive(Debug, Deserialize)]
#[serde(tag = "messageType", rename_all = "snake_case")]
pub enum ServerMessage {
    ClientResponse(ClientResponseMessage),
    EventSubscription(EventSubscriptionMessage),
}

/// Response to a query or command sent by this client.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientResponseMessage {
    pub client_message: String,
    pub data: Option<ClientResponseData>,
    pub error: Option<String>,
    pub success: bool,
}

/// Typed payload of a client response.
///
/// Only the variants actually needed by glazeid are listed. Unknown variants
/// are ignored via the `#[serde(other)]` fallback.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum ClientResponseData {
    Monitors(MonitorsData),
    EventSubscribe(EventSubscribeData),
    /// Catch-all for query types we don't consume.
    Other(()),
}

/// Payload of a `query monitors` response.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MonitorsData {
    pub monitors: Vec<ContainerDto>,
}

/// Payload of a `sub` response confirming the subscription ID.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EventSubscribeData {
    pub subscription_id: Uuid,
}

// ---------------------------------------------------------------------------
// Event subscription message
// ---------------------------------------------------------------------------

/// Server-push event delivered to active subscriptions.
#[allow(dead_code)]
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EventSubscriptionMessage {
    pub data: Option<WmEvent>,
    pub error: Option<String>,
    pub subscription_id: Uuid,
    pub success: bool,
}

/// Subset of GlazeWM events that affect workspace state.
#[allow(dead_code)]
#[derive(Debug, Deserialize)]
#[serde(
    tag = "eventType",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum WmEvent {
    WorkspaceActivated {
        activated_workspace: ContainerDto,
    },
    WorkspaceDeactivated {
        deactivated_id: Uuid,
        deactivated_name: String,
    },
    WorkspaceUpdated {
        updated_workspace: ContainerDto,
    },
    FocusChanged {
        focused_container: ContainerDto,
    },
    FocusedContainerMoved {
        focused_container: ContainerDto,
    },
    MonitorAdded {
        added_monitor: ContainerDto,
    },
    MonitorRemoved {
        removed_id: Uuid,
        removed_device_name: String,
    },
    MonitorUpdated {
        updated_monitor: ContainerDto,
    },
    /// Any other event type we don't need to handle — parsed but ignored.
    #[serde(other)]
    Other,
}

// ---------------------------------------------------------------------------
// Container DTOs
// ---------------------------------------------------------------------------

/// Discriminated-union container representation used in IPC responses.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContainerDto {
    Root(RootContainerDto),
    Monitor(MonitorDto),
    Workspace(WorkspaceDto),
    Split(SplitContainerDto),
    Window(WindowDto),
}

impl ContainerDto {
    /// Extract the inner `WorkspaceDto` if this is a workspace container.
    pub fn as_workspace(&self) -> Option<&WorkspaceDto> {
        match self {
            Self::Workspace(ws) => Some(ws),
            _ => None,
        }
    }

    /// Extract the inner `MonitorDto` if this is a monitor container.
    pub fn as_monitor(&self) -> Option<&MonitorDto> {
        match self {
            Self::Monitor(m) => Some(m),
            _ => None,
        }
    }
}

/// Root container (single, wraps all monitors).
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RootContainerDto {
    pub id: Uuid,
}

/// Monitor container.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MonitorDto {
    pub id: Uuid,
    pub children: Vec<ContainerDto>,
    pub has_focus: bool,
    pub width: i32,
    pub height: i32,
    pub x: i32,
    pub y: i32,
    pub scale_factor: f32,
    pub device_name: String,
}

/// Workspace container.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceDto {
    pub id: Uuid,
    pub name: String,
    pub display_name: Option<String>,
    pub parent_id: Option<Uuid>,
    pub has_focus: bool,
    pub is_displayed: bool,
}

/// Split container (tiling subtree). Fields unused by glazeid.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SplitContainerDto {
    pub id: Uuid,
}

/// Window container. Fields unused by glazeid.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WindowDto {
    pub id: Uuid,
}
