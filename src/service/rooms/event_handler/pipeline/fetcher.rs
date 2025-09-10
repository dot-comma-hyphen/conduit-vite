use std::{collections::BTreeMap, sync::Arc};

use ruma::{room_version_rules::RoomVersionRules, RoomId, ServerName};
use tokio::sync::RwLock;
use tracing::debug;

use crate::{
    service::{globals::SigningKeys, pdu::PduEvent, rooms::event_handler::Service},
    Result,
};

pub(in crate::service::rooms::event_handler) async fn fetch_dependencies<'a>(
    event_handler: &'a Service,
    origin: &'a ServerName,
    pdu: &PduEvent,
    create_event: &'a PduEvent,
    room_id: &'a RoomId,
    room_version_rules: &'a RoomVersionRules,
    pub_key_map: &'a RwLock<BTreeMap<String, SigningKeys>>,
) -> Result<()> {
    debug!(event_id = ?pdu.event_id, "Fetching auth events");
    event_handler
        .fetch_and_handle_outliers(
            origin,
            &pdu.auth_events
                .iter()
                .map(|x| Arc::from(&**x))
                .collect::<Vec<_>>(),
            create_event,
            room_id,
            room_version_rules,
            pub_key_map,
        )
        .await;

    Ok(())
}
