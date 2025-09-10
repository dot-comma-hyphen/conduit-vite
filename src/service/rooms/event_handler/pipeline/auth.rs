use std::collections::HashMap;

use ruma::{
    api::client::error::ErrorKind, events::StateEventType, room_version_rules::RoomVersionRules,
    state_res, EventId,
};
use tracing::{debug, warn};

use crate::{service::pdu::PduEvent, services, Error, Result};

pub(in crate::service::rooms::event_handler) fn authorize_pdu(
    pdu: &PduEvent,
    room_version_rules: &RoomVersionRules,
) -> Result<()> {
    debug!("Auth check for {} based on auth events", pdu.event_id);

    // Build map of auth events
    let mut auth_events = HashMap::new();
    let mut auth_events_by_event_id = HashMap::new();

    let insert_auth_event = |auth_events: &mut HashMap<_, _>,
                             auth_events_by_event_id: &mut HashMap<_, _>,
                             id|
     -> Result<()> {
        let auth_event = match services().rooms.timeline.get_pdu(id)? {
            Some(e) => e,
            None => {
                warn!("Could not find auth event {}", id);
                return Ok(());
            }
        };

        auth_events_by_event_id.insert(auth_event.event_id.clone(), auth_event.clone());
        auth_events.insert(
            (
                StateEventType::from(auth_event.kind.to_string()),
                auth_event
                    .state_key
                    .clone()
                    .expect("all auth events have state keys"),
            ),
            auth_event,
        );

        Ok(())
    };

    for id in &pdu.auth_events {
        insert_auth_event(&mut auth_events, &mut auth_events_by_event_id, id)?;
    }

    // Create event is always needed to authorize any event (besides the create events itself)
    if room_version_rules
        .authorization
        .room_create_event_id_as_room_id
    {
        if let Some(room_id) = &pdu.room_id {
            let event_id = EventId::parse(format!("${}", room_id.strip_sigil())).map_err(|_| {
                Error::BadRequest(
                    ErrorKind::InvalidParam,
                    "Room ID cannot be converted to event ID, despite room version rules requiring so.",
                )
            })?;
            insert_auth_event(&mut auth_events, &mut auth_events_by_event_id, &event_id)?;
        }
    }

    // first time we are doing any sort of auth check, so we check state-independent
    // auth rules in addition to the state-dependent ones.
    if state_res::check_state_independent_auth_rules(
        &room_version_rules.authorization,
        pdu,
        |event_id| auth_events_by_event_id.get(event_id),
    )
    .is_err()
        || state_res::check_state_dependent_auth_rules(
            &room_version_rules.authorization,
            pdu,
            |k, s| auth_events.get(&(k.to_string().into(), s.to_owned())),
        )
        .is_err()
    {
        return Err(Error::BadRequest(
            ErrorKind::InvalidParam,
            "Auth check failed",
        ));
    }

    Ok(())
}
