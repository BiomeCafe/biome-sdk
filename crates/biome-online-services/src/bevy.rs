//! Bevy plugin (feature = "bevy", pinned to bevy 0.16).
//!
//! Wires a [`BiomeClient`] into a Bevy app without ever blocking it: the
//! connect (and the first-run consent prompt, which can take minutes) runs in
//! the background and an exclusive system polls it. Once connected, inbound
//! events are pumped into [`BosEvent`] every frame.
//!
//! ```ignore
//! use biome_online_services::BosConfig;
//! use biome_online_services::bevy::{BosClient, BosEvent, BosPlugin, BosStatus};
//!
//! app.add_plugins(BosPlugin::new(BosConfig::new("cafe-shooter", "Cafe Shooter")));
//!
//! fn menu(status: Res<BosStatus>, client: Option<NonSend<BosClient>>) {
//!     // status.0: Connecting → AwaitingConsent → Done
//!     // client is Some once connected; BosInitError is inserted on failure.
//! }
//! fn handle_bos(mut events: EventReader<BosEvent>) {
//!     for ev in events.read() { /* ... */ }
//! }
//! ```
//!
//! On another Bevy version, copy this file: it is the whole integration.

use bevy_app::{App, Plugin, Update};
use bevy_ecs::event::{Event, EventWriter};
use bevy_ecs::prelude::*;

use crate::client::{BiomeClient, ConnectOptions, ConnectStatus, PendingClient};
use crate::events::BiomeEvent;
use crate::types::BosConfig;

/// Bevy event wrapper. We can't directly derive `Event` on the upstream
/// `BiomeEvent` because it lives in this crate without a bevy dep by default.
/// Use this newtype when reading from a system.
#[derive(Event, Debug, Clone)]
pub struct BosEvent(pub BiomeEvent);

/// The connected client, as a NonSend resource (it owns a tokio runtime
/// thread and channels). Present only after the connect succeeded.
pub struct BosClient(pub BiomeClient);

/// The connect in progress (NonSend). Removed when it finishes either way.
pub struct BosPending(pub PendingClient);

/// Where the connect is at. Drive "waiting for Biome…" UI from this.
#[derive(Resource, Debug, Clone, Copy, PartialEq, Eq)]
pub struct BosStatus(pub ConnectStatus);

/// Inserted when the connect failed, so a system can surface it gracefully
/// (e.g. "Biome not running", "sign in to Biome") rather than the app
/// panicking at boot.
#[derive(Resource, Debug)]
pub struct BosInitError(pub String);

/// Bevy plugin: start connecting in the background, install the
/// [`BiomeClient`] when it is ready, and drain its events into [`BosEvent`]
/// every frame.
pub struct BosPlugin {
    config: BosConfig,
    options: ConnectOptions,
}

impl BosPlugin {
    pub fn new(config: BosConfig) -> Self {
        Self { config, options: ConnectOptions::default() }
    }

    pub fn with_options(mut self, options: ConnectOptions) -> Self {
        self.options = options;
        self
    }
}

impl Plugin for BosPlugin {
    fn build(&self, app: &mut App) {
        app.add_event::<BosEvent>();
        app.insert_resource(BosStatus(ConnectStatus::Connecting));
        // Returns immediately; the handshake runs on a helper thread.
        app.insert_non_send_resource(BosPending(BiomeClient::connect_background(
            self.config.clone(),
            self.options.clone(),
        )));
        app.add_systems(Update, (poll_bos_connect, drain_bos_events).chain());
    }
}

/// Exclusive: moves the client from "pending" to "installed" when ready.
fn poll_bos_connect(world: &mut World) {
    let (status, polled) = match world.get_non_send_resource_mut::<BosPending>() {
        Some(mut pending) => (pending.0.status(), pending.0.poll()),
        None => return,
    };
    if let Some(mut current) = world.get_resource_mut::<BosStatus>() {
        if current.0 != status {
            current.0 = status;
        }
    }
    match polled {
        None => {}
        Some(Ok(client)) => {
            world.remove_non_send_resource::<BosPending>();
            world.insert_non_send_resource(BosClient(client));
            world.insert_resource(BosStatus(ConnectStatus::Done));
        }
        Some(Err(e)) => {
            world.remove_non_send_resource::<BosPending>();
            tracing::warn!("BOS plugin: connect failed: {e}");
            world.insert_resource(BosInitError(e.to_string()));
            world.insert_resource(BosStatus(ConnectStatus::Done));
        }
    }
}

fn drain_bos_events(client: Option<NonSend<BosClient>>, mut writer: EventWriter<BosEvent>) {
    let Some(client) = client else { return };
    for ev in client.0.drain_events() {
        writer.write(BosEvent(ev));
    }
}
