use std::collections::BTreeMap;

use ruma::{
    canonical_json::{redact, CanonicalJsonValue},
    room_version_rules::RoomVersionRules,
    signatures::Verified,
    state_res, MilliSecondsSinceUnixEpoch,
};
use tokio::sync::RwLock;
use tracing::{error, warn};

use crate::{
    service::{
        globals::SigningKeys,
        pdu::PduEvent,
        rooms::event_handler::{self, Service},
    },
    services, Error, Result,
};

/// Validates a PDU, checking format, signatures, and content hash.
///
/// This function is responsible for the initial, stateless validation of a PDU. It ensures that the
/// event is well-formed, properly signed by the origin server, and that its content hash is correct.
///
/// # Arguments
///
/// * `event_handler` - The event handler service.
/// * `pdu` - The PDU event to validate.
/// * `room_version_rules` - The room version rules.
/// * `pub_key_map` - The public key map.
///
/// # Returns
///
/// * `Ok(BTreeMap<String, CanonicalJsonValue>)` - The validated and potentially redacted PDU as a
///   BTreeMap.
/// * `Err(Error)` - If the PDU is invalid.
pub(in crate::service::rooms::event_handler) async fn validate_pdu(
    event_handler: &Service,
    pdu: &PduEvent,
    room_version_rules: &RoomVersionRules,
    pub_key_map: &RwLock<BTreeMap<String, SigningKeys>>,
) -> Result<BTreeMap<String, CanonicalJsonValue>> {
    let mut value: BTreeMap<String, CanonicalJsonValue> =
        serde_json::from_str(pdu.content.get())
            .map_err(|_| Error::bad_database("Event content is invalid JSON."))?;

    // 1.1. Remove unsigned field
    value.remove("unsigned");

    if let Err(e) = state_res::check_pdu_format(&value, &room_version_rules.event_format) {
        warn!("Invalid PDU with event ID {} received: {}", pdu.event_id, e);
        return Err(Error::BadRequest(
            ruma::api::client::error::ErrorKind::InvalidParam,
            "Received Invalid PDU",
        ));
    }

    event_handler
        .fetch_required_signing_keys(&value, pub_key_map)
        .await?;

    let origin_server_ts = value.get("origin_server_ts").ok_or_else(|| {
        error!("Invalid PDU, no origin_server_ts field");
        Error::BadRequest(
            ruma::api::client::error::ErrorKind::MissingParam,
            "Invalid PDU, no origin_server_ts field",
        )
    })?;

    let origin_server_ts: MilliSecondsSinceUnixEpoch = {
        let ts = origin_server_ts.as_integer().ok_or_else(|| {
            Error::BadRequest(
                ruma::api::client::error::ErrorKind::InvalidParam,
                "origin_server_ts must be an integer",
            )
        })?;

        MilliSecondsSinceUnixEpoch(i64::from(ts).try_into().map_err(|_| {
            Error::BadRequest(
                ruma::api::client::error::ErrorKind::InvalidParam,
                "Time must be after the unix epoch",
            )
        })?)
    };

    let guard = pub_key_map.read().await;

    let pkey_map = (*guard).clone();

    let filtered_keys =
        services()
            .globals
            .filter_keys_server_map(pkey_map, origin_server_ts, room_version_rules);

    let val = match ruma::signatures::verify_event(&filtered_keys, &value, room_version_rules) {
        Err(e) => {
            warn!("Dropping bad event {}: {}", pdu.event_id, e,);
            return Err(Error::BadRequest(
                ruma::api::client::error::ErrorKind::InvalidParam,
                "Signature verification failed",
            ));
        }
        Ok(Verified::Signatures) => {
            warn!("Calculated hash does not match: {}", pdu.event_id);
            let obj = match redact(value, &room_version_rules.redaction, None) {
                Ok(obj) => obj,
                Err(_) => {
                    return Err(Error::BadRequest(
                        ruma::api::client::error::ErrorKind::InvalidParam,
                        "Redaction failed",
                    ))
                }
            };

            if services()
                .rooms
                .timeline
                .get_pdu_json(&pdu.event_id)?
                .is_some()
            {
                return Err(Error::BadRequest(
                    ruma::api::client::error::ErrorKind::InvalidParam,
                    "Event was redacted and we already knew about it",
                ));
            }

            obj
        }
        Ok(Verified::All) => value,
    };

    drop(guard);

    Ok(val)
}
