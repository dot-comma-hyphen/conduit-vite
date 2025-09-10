use std::{
    collections::{hash_map, BTreeMap, HashMap},
    sync::Arc,
};

use ruma::{
    api::federation::event::get_room_state_ids,
    room_version_rules::RoomVersionRules,
    state_res::{self, StateMap},
    EventId, RoomId, ServerName,
};
use tokio::sync::RwLock;
use tracing::{debug, error, warn};

use crate::{
    service::{globals::SigningKeys, pdu::PduEvent, rooms::event_handler::Service},
    services, Error, PduEvent as ConduitPduEvent, Result,
};

#[allow(clippy::too_many_arguments)]
pub(in crate::service::rooms::event_handler) async fn resolve_state(
    event_handler: &Service,
    origin: &ServerName,
    pdu: &ConduitPduEvent,
    create_event: &ConduitPduEvent,
    room_id: &RoomId,
    room_version_rules: &RoomVersionRules,
    pub_key_map: &RwLock<BTreeMap<String, SigningKeys>>,
) -> Result<HashMap<u64, Arc<EventId>>> {
    // This is a large chunk of logic from the original `upgrade_outlier_to_timeline_pdu`
    debug!("Requesting state at event");
    let mut state_at_incoming_event = None;

    if pdu.prev_events.len() == 1 {
        let prev_event = &*pdu.prev_events[0];
        let prev_event_sstatehash = services()
            .rooms
            .state_accessor
            .pdu_shortstatehash(prev_event)?;

        let state = if let Some(shortstatehash) = prev_event_sstatehash {
            Some(
                services()
                    .rooms
                    .state_accessor
                    .state_full_ids(shortstatehash)
                    .await,
            )
        } else {
            None
        };

        if let Some(Ok(mut state)) = state {
            debug!("Using cached state");
            let prev_pdu = services()
                .rooms
                .timeline
                .get_pdu(prev_event)
                .ok()
                .flatten()
                .ok_or_else(|| {
                    Error::bad_database("Could not find prev event, but we know the state.")
                })?;

            if let Some(state_key) = &prev_pdu.state_key {
                let shortstatekey = services()
                    .rooms
                    .short
                    .get_or_create_shortstatekey(&prev_pdu.kind.to_string().into(), state_key)?;

                state.insert(shortstatekey, Arc::from(prev_event));
                // Now it's the state after the pdu
            }

            state_at_incoming_event = Some(state);
        }
    } else {
        debug!("Calculating state at event using state res");
        let mut extremity_sstatehashes = HashMap::new();

        let mut okay = true;
        for prev_eventid in &pdu.prev_events {
            let prev_event = if let Ok(Some(pdu)) = services().rooms.timeline.get_pdu(prev_eventid)
            {
                pdu
            } else {
                okay = false;
                break;
            };

            let sstatehash = if let Ok(Some(s)) = services()
                .rooms
                .state_accessor
                .pdu_shortstatehash(prev_eventid)
            {
                s
            } else {
                okay = false;
                break;
            };

            extremity_sstatehashes.insert(sstatehash, prev_event);
        }

        if okay {
            let mut fork_states = Vec::with_capacity(extremity_sstatehashes.len());
            let mut auth_chain_sets = Vec::with_capacity(extremity_sstatehashes.len());

            for (sstatehash, prev_event) in extremity_sstatehashes {
                let mut leaf_state: HashMap<_, _> = services()
                    .rooms
                    .state_accessor
                    .state_full_ids(sstatehash)
                    .await?;

                if let Some(state_key) = &prev_event.state_key {
                    let shortstatekey = services().rooms.short.get_or_create_shortstatekey(
                        &prev_event.kind.to_string().into(),
                        state_key,
                    )?;
                    leaf_state.insert(shortstatekey, Arc::from(&*prev_event.event_id));
                }

                let mut state = StateMap::with_capacity(leaf_state.len());
                let mut starting_events = Vec::with_capacity(leaf_state.len());

                for (k, id) in leaf_state {
                    if let Ok((ty, st_key)) = services().rooms.short.get_statekey_from_short(k) {
                        state.insert((ty.to_string().into(), st_key), id.clone());
                    } else {
                        warn!("Failed to get_statekey_from_short.");
                    }
                    starting_events.push(id);
                }

                auth_chain_sets.push(
                    services()
                        .rooms
                        .auth_chain
                        .get_auth_chain(room_id, starting_events)
                        .await?
                        .collect(),
                );

                fork_states.push(state);
            }

            let lock = services().globals.stateres_mutex.lock();

            let result = state_res::resolve(
                &room_version_rules.authorization,
                room_version_rules
                    .state_res
                    .v2_rules()
                    .expect("We only support room versions using state resolution v2"),
                &fork_states,
                auth_chain_sets,
                |id| services().rooms.timeline.get_pdu(id).ok().flatten(),
                |css| {
                    services()
                        .rooms
                        .auth_chain
                        .get_conflicted_state_subgraph(room_id, css)
                        .ok()
                },
            );
            drop(lock);

            state_at_incoming_event = match result {
                Ok(new_state) => Some(
                    new_state
                        .into_iter()
                        .map(|((event_type, state_key), event_id)| {
                            let shortstatekey =
                                services().rooms.short.get_or_create_shortstatekey(
                                    &event_type.to_string().into(),
                                    &state_key,
                                )?;
                            Ok((shortstatekey, event_id))
                        })
                        .collect::<Result<_>>()?,
                ),
                Err(e) => {
                    warn!("State resolution on prev events failed: {}", e);
                    None
                }
            }
        }
    }

    if state_at_incoming_event.is_none() {
        debug!("Calling /state_ids");
        match services()
            .sending
            .send_federation_request(
                origin,
                get_room_state_ids::v1::Request {
                    room_id: room_id.to_owned(),
                    event_id: (*pdu.event_id).to_owned(),
                },
            )
            .await
        {
            Ok(res) => {
                debug!("Fetching state events at event.");
                let state_vec = event_handler
                    .fetch_and_handle_outliers(
                        origin,
                        &res.pdu_ids
                            .iter()
                            .map(|id| Arc::from(&**id))
                            .collect::<Vec<_>>(),
                        create_event,
                        room_id,
                        room_version_rules,
                        pub_key_map,
                    )
                    .await;

                let mut state: HashMap<_, Arc<EventId>> = HashMap::new();
                for (pdu, _) in state_vec {
                    let state_key = pdu.state_key.clone().ok_or_else(|| {
                        Error::bad_database("Found non-state pdu in state events.")
                    })?;

                    let shortstatekey = services()
                        .rooms
                        .short
                        .get_or_create_shortstatekey(&pdu.kind.to_string().into(), &state_key)?;

                    match state.entry(shortstatekey) {
                        hash_map::Entry::Vacant(v) => {
                            v.insert(Arc::from(&*pdu.event_id));
                        }
                        hash_map::Entry::Occupied(_) => return Err(Error::bad_database(
                            "State event's type and state_key combination exists multiple times.",
                        )),
                    }
                }

                let create_shortstatekey = services()
                    .rooms
                    .short
                    .get_shortstatekey(&ruma::events::StateEventType::RoomCreate, "")?
                    .expect("Room exists");

                if state.get(&create_shortstatekey).map(|id| id.as_ref())
                    != Some(&create_event.event_id)
                {
                    return Err(Error::bad_database(
                        "Incoming event refers to wrong create event.",
                    ));
                }

                state_at_incoming_event = Some(state);
            }
            Err(e) => {
                warn!("Fetching state for event failed: {}", e);
                return Err(e);
            }
        };
    }

    Ok(state_at_incoming_event.expect("we always set this to some above"))
}
