//! The parts of the settings that change while the server runs (ADR-0004 §4): the access
//! directory (nodes, roles, principals) and the peers.
//!
//! They are swapped as one, because nodes name peers: a reader always sees a directory and the
//! peers it was validated with. Readers take a snapshot per call ([`Live::load`]) and keep using it
//! until the call is done, so a call never sees half of a change.

use std::collections::BTreeMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use medicus_core::capability::Peer;

use crate::access::Directory;
use crate::config::Config;

#[derive(Debug)]
pub(crate) struct LiveState {
    pub(crate) directory: Directory,
    pub(crate) peers: BTreeMap<String, Peer>,
}

impl LiveState {
    /// Builds and validates the state of `cfg`.
    pub(crate) fn from_config(cfg: &Config) -> anyhow::Result<Self> {
        Ok(Self {
            directory: Directory::from_config(cfg)?,
            peers: cfg
                .peers
                .iter()
                .map(|(name, endpoint)| {
                    (
                        name.clone(),
                        Peer {
                            name: name.clone(),
                            endpoint: endpoint.clone(),
                        },
                    )
                })
                .collect(),
        })
    }
}

#[derive(Debug)]
pub(crate) struct Live(ArcSwap<LiveState>);

impl Live {
    pub(crate) fn new(state: LiveState) -> Self {
        Self(ArcSwap::from_pointee(state))
    }

    pub(crate) fn from_config(cfg: &Config) -> anyhow::Result<Self> {
        LiveState::from_config(cfg).map(Self::new)
    }

    /// The current state.
    pub(crate) fn load(&self) -> Arc<LiveState> {
        self.0.load_full()
    }

    /// Replaces the state; calls already running keep the one they loaded.
    pub(crate) fn store(&self, state: LiveState) {
        self.0.store(Arc::new(state));
    }
}
