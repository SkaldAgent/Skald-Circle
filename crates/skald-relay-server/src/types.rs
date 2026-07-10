//! Wire-protocol types re-exported from the shared `skald-relay-common` crate
//! (see plugin.md §1.1). The relay and the mobile-connector plugin use the
//! same byte-level frames so they can never diverge.
//!
//! - **v2 (current)**: [`proto`] — protobuf types for the binary WebSocket
//!   transport (data/ios-app/v2/relay-protocol.md). Every wire frame is a
//!   `RelayFrame` carrying one of the sub-messages (Challenge, Auth, Message,
//!   PresenceEvent, …). This is the only transport the relay speaks now.

/// v2 protobuf frames — namespaced. The WS layer reads/writes these.
pub mod proto {
    pub use skald_relay_common::proto::v2::*;
}

#[cfg(test)]
mod tests {
    #[test]
    fn v2_proto_types_exposed() {
        let _v2_frame: skald_relay_common::proto::v2::RelayFrame =
            skald_relay_common::proto::v2::RelayFrame { frame: None };
    }
}
