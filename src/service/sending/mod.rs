mod data;

pub use data::Data;

use std::{
    collections::{BTreeMap, HashSet},
    fmt::Debug,
    sync::Arc,
    time::Duration,
};

use crate::{
    api::{appservice_server, server_server},
    services,
    utils::calculate_hash,
    Config, Error, PduEvent, Result,
};
use federation::transactions::send_transaction_message;

use base64::{engine::general_purpose, Engine as _};

use ruma::{
    api::{
        appservice::{self, Registration},
        federation::{
            self,
            transactions::edu::{
                DeviceListUpdateContent, Edu, ReceiptContent, ReceiptData, ReceiptMap,
            },
        },
        OutgoingRequest,
    },
    device_id,
    events::{
        push_rules::PushRulesEvent, receipt::ReceiptType, AnySyncEphemeralRoomEvent,
        GlobalAccountDataEventType,
    },
    push, uint, MilliSecondsSinceUnixEpoch, OwnedRoomId, OwnedServerName, OwnedUserId, ServerName,
    UInt, UserId,
};
use tokio::{
    select,
    sync::{mpsc, Mutex, RwLock, Semaphore},
};
use tracing::{debug, error, warn};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum OutgoingKind {
    Appservice(String),
    Push(OwnedUserId, String), // user and pushkey
    Normal(OwnedServerName),
}

impl OutgoingKind {
    #[tracing::instrument(skip(self))]
    pub fn get_prefix(&self) -> Vec<u8> {
        let mut prefix = match self {
            OutgoingKind::Appservice(server) => {
                let mut p = b"+".to_vec();
                p.extend_from_slice(server.as_bytes());
                p
            }
            OutgoingKind::Push(user, pushkey) => {
                let mut p = b"$".to_vec();
                p.extend_from_slice(user.as_bytes());
                p.push(0xff);
                p.extend_from_slice(pushkey.as_bytes());
                p
            }
            OutgoingKind::Normal(server) => {
                let mut p = Vec::new();
                p.extend_from_slice(server.as_bytes());
                p
            }
        };
        prefix.push(0xff);

        prefix
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum SendingEventType {
    Pdu(Vec<u8>), // pduid
    Edu(Vec<u8>), // pdu json
}

pub struct Service {
    db: &'static dyn Data,
    pub federation_typers_stop:
        RwLock<BTreeMap<OwnedServerName, BTreeMap<OwnedRoomId, Vec<OwnedUserId>>>>,

    /// The state for a given state hash.
    pub(super) maximum_requests: Arc<Semaphore>,
    pub sender: mpsc::UnboundedSender<(OutgoingKind, SendingEventType, Vec<u8>)>,
    receiver: Mutex<mpsc::UnboundedReceiver<(OutgoingKind, SendingEventType, Vec<u8>)>>,
}

impl Service {
    pub fn build(db: &'static dyn Data, config: &Config) -> Arc<Self> {
        let (sender, receiver) = mpsc::unbounded_channel();
        Arc::new(Self {
            db,
            sender,
            receiver: Mutex::new(receiver),
            federation_typers_stop: RwLock::new(BTreeMap::new()),
            maximum_requests: Arc::new(Semaphore::new(config.max_concurrent_requests as usize)),
        })
    }

    pub fn start_handler(self: &Arc<Self>) {
        let self2 = Arc::clone(self);
        tokio::spawn(async move {
            self2.handler().await.unwrap();
        });
    }

    async fn handler(self: Arc<Self>) -> Result<()> {
        let mut receiver = self.receiver.lock().await;
        let running_destinations = Arc::new(Mutex::new(HashSet::new()));
        let mut interval = tokio::time::interval(Duration::from_millis(800));

        loop {
            select! {
                _ = interval.tick() => {
                    // Proactively spawn workers for all known destinations to send fresh EDUs.
                    let mut destinations: HashSet<OwnedServerName> = services()
                        .rooms
                        .state_cache
                        .rooms_joined(services().globals.server_user())
                        .filter_map(Result::ok)
                        .flat_map(|room_id| {
                            services()
                                .rooms
                                .state_cache
                                .room_servers(&room_id)
                                .filter_map(Result::ok)
                        })
                        .collect();
                    destinations.remove(services().globals.server_name());

                    let running_destinations_lock = running_destinations.lock().await;
                    for dest in destinations {
                        let outgoing_kind = OutgoingKind::Normal(dest);
                        if !running_destinations_lock.contains(&outgoing_kind) {
                            Arc::clone(&self).spawn_worker(outgoing_kind, Arc::clone(&running_destinations));
                        }
                    }
                }
                Some((outgoing_kind, event, _key)) = receiver.recv() => {
                    self.db.queue_requests(&[(&outgoing_kind, event)])?;
                    let running_destinations_lock = running_destinations.lock().await;
                    if !running_destinations_lock.contains(&outgoing_kind) {
                        Arc::clone(&self).spawn_worker(outgoing_kind, Arc::clone(&running_destinations));
                    }
                }
            }
        }
    }

    fn spawn_worker(
        self: Arc<Self>,
        outgoing_kind: OutgoingKind,
        running_destinations: Arc<Mutex<HashSet<OutgoingKind>>>,
    ) {
        tokio::spawn(async move {
            running_destinations
                .lock()
                .await
                .insert(outgoing_kind.clone());

            self.worker(outgoing_kind.clone()).await;
            running_destinations.lock().await.remove(&outgoing_kind);
        });
    }

    async fn worker(&self, outgoing_kind: OutgoingKind) {
        loop {
            let mut active_events = self
                .db
                .active_requests_for(&outgoing_kind)
                .filter_map(Result::ok)
                .collect::<Vec<_>>();

            let new_events = self
                .db
                .queued_requests(&outgoing_kind)
                .filter_map(Result::ok)
                .collect::<Vec<_>>();

            let mut selected_edus = Vec::new();
            if let OutgoingKind::Normal(server_name) = &outgoing_kind {
                if let Ok(edus) = self.select_edus(server_name).await {
                    selected_edus = edus;
                }
            }

            if active_events.is_empty() && new_events.is_empty() && selected_edus.is_empty() {
                break;
            }

            let had_db_events = !active_events.is_empty() || !new_events.is_empty();

            if let Err(e) = self.db.mark_as_active(&new_events) {
                error!("Failed to mark new events as active, trying again later: {e}");
                break;
            }

            active_events.extend(new_events.into_iter().map(|(e, k)| (k, e)));

            let mut events_to_send = active_events
                .into_iter()
                .map(|(_key, event)| event)
                .collect::<Vec<_>>();

            events_to_send.extend(
                selected_edus
                    .into_iter()
                    .map(|edu| SendingEventType::Edu(serde_json::to_vec(&edu).unwrap())),
            );

            let result = Self::handle_events(outgoing_kind.clone(), events_to_send).await;

            if result.is_ok() {
                if let Err(e) = self.db.delete_all_active_requests_for(&outgoing_kind) {
                    error!(
                        "Failed to delete active requests for {:?}, trying again later: {e}",
                        outgoing_kind
                    );
                    break;
                }

                if !had_db_events {
                    break;
                }
            } else {
                break;
            }
        }
    }

    #[tracing::instrument(skip(self, server_name))]
    pub async fn select_edus(&self, server_name: &ServerName) -> Result<Vec<Edu>> {
        let mut events = Vec::new();
        let mut device_list_changes = HashSet::new();

        'outer: for room_id in services()
            .rooms
            .state_cache
            .server_rooms(server_name)
            .collect::<Result<Vec<_>>>()?
        {
            // Look for device list updates in this room
            device_list_changes.extend(
                services()
                    .users
                    .keys_changed(room_id.as_ref(), 0, None)
                    .filter_map(|r| r.ok())
                    .filter(|user_id| user_id.server_name() == services().globals.server_name()),
            );

            // Add typing events
            let typing_users = services().rooms.edus.typing.typings_all(&room_id).await?;
            if !typing_users.content.user_ids.is_empty() {
                for user_id in typing_users.content.user_ids {
                    events.push(Edu::Typing(federation::transactions::edu::TypingContent {
                        room_id: room_id.clone(),
                        user_id,
                        typing: true,
                    }));
                }
            }

            // Look for read receipts in this room
            for r in services()
                .rooms
                .edus
                .read_receipt
                .readreceipts_since(&room_id, 0)
            {
                let (user_id, _count, read_receipt) = r?;

                if user_id.server_name() != services().globals.server_name() {
                    continue;
                }

                let event: AnySyncEphemeralRoomEvent =
                    serde_json::from_str(read_receipt.json().get())
                        .map_err(|_| Error::bad_database("Invalid edu event in read_receipts."))?;
                let federation_event = match event {
                    AnySyncEphemeralRoomEvent::Receipt(r) => {
                        let mut read = BTreeMap::new();

                        let (event_id, mut receipt) = r
                            .content
                            .0
                            .into_iter()
                            .next()
                            .expect("we only use one event per read receipt");
                        let receipt = receipt
                            .remove(&ReceiptType::Read)
                            .expect("our read receipts always set this")
                            .remove(&user_id)
                            .expect("our read receipts always have the user here");

                        read.insert(
                            user_id,
                            ReceiptData {
                                data: receipt.clone(),
                                event_ids: vec![event_id.clone()],
                            },
                        );

                        let receipt_map = ReceiptMap { read };

                        let mut receipts = BTreeMap::new();
                        receipts.insert(room_id.clone(), receipt_map);

                        Edu::Receipt(ReceiptContent { receipts })
                    }
                    _ => {
                        Error::bad_database("Invalid event type in read_receipts");
                        continue;
                    }
                };

                events.push(federation_event);

                if events.len() >= 20 {
                    break 'outer;
                }
            }
        }

        if let Some(stopped_typing) = self
            .federation_typers_stop
            .write()
            .await
            .remove(server_name)
        {
            for (room_id, users) in stopped_typing {
                for user_id in users {
                    events.push(Edu::Typing(federation::transactions::edu::TypingContent {
                        room_id: room_id.clone(),
                        user_id,
                        typing: false,
                    }));
                }
            }
        }

        for user_id in device_list_changes {
            // Empty prev id forces synapse to resync: https://github.com/matrix-org/synapse/blob/98aec1cc9da2bd6b8e34ffb282c85abf9b8b42ca/synapse/handlers/device.py#L767
            // Because synapse resyncs, we can just insert dummy data
            let edu = Edu::DeviceListUpdate(DeviceListUpdateContent {
                user_id,
                device_id: device_id!("dummy").to_owned(),
                device_display_name: Some("Dummy".to_owned()),
                stream_id: uint!(1),
                prev_id: Vec::new(),
                deleted: None,
                keys: None,
            });

            events.push(edu);
        }

        Ok(events)
    }

    #[tracing::instrument(skip(self, pdu_id, user, pushkey))]
    pub fn send_push_pdu(&self, pdu_id: &[u8], user: &UserId, pushkey: String) -> Result<()> {
        let outgoing_kind = OutgoingKind::Push(user.to_owned(), pushkey);
        let event = SendingEventType::Pdu(pdu_id.to_owned());
        let keys = self.db.queue_requests(&[(&outgoing_kind, event.clone())])?;
        self.sender
            .send((outgoing_kind, event, keys.into_iter().next().unwrap()))
            .unwrap();

        Ok(())
    }

    #[tracing::instrument(skip(self, servers, pdu_id))]
    pub fn send_pdu<I: Iterator<Item = OwnedServerName>>(
        &self,
        servers: I,
        pdu_id: &[u8],
    ) -> Result<()> {
        let requests = servers
            .into_iter()
            .map(|server| {
                (
                    OutgoingKind::Normal(server),
                    SendingEventType::Pdu(pdu_id.to_owned()),
                )
            })
            .collect::<Vec<_>>();
        let keys = self.db.queue_requests(
            &requests
                .iter()
                .map(|(o, e)| (o, e.clone()))
                .collect::<Vec<_>>(),
        )?;
        for ((outgoing_kind, event), key) in requests.into_iter().zip(keys) {
            self.sender
                .send((outgoing_kind.to_owned(), event, key))
                .unwrap();
        }

        Ok(())
    }

    #[tracing::instrument(skip(self, server, serialized))]
    pub fn send_reliable_edu(
        &self,
        server: &ServerName,
        serialized: Vec<u8>,
        id: u64,
    ) -> Result<()> {
        let outgoing_kind = OutgoingKind::Normal(server.to_owned());
        let event = SendingEventType::Edu(serialized);
        let keys = self.db.queue_requests(&[(&outgoing_kind, event.clone())])?;
        self.sender
            .send((outgoing_kind, event, keys.into_iter().next().unwrap()))
            .unwrap();

        Ok(())
    }

    #[tracing::instrument(skip(self))]
    pub fn send_pdu_appservice(&self, appservice_id: String, pdu_id: Vec<u8>) -> Result<()> {
        let outgoing_kind = OutgoingKind::Appservice(appservice_id);
        let event = SendingEventType::Pdu(pdu_id);
        let keys = self.db.queue_requests(&[(&outgoing_kind, event.clone())])?;
        self.sender
            .send((outgoing_kind, event, keys.into_iter().next().unwrap()))
            .unwrap();

        Ok(())
    }

    /// Cleanup event data
    /// Used for instance after we remove an appservice registration
    ///
    #[tracing::instrument(skip(self))]
    pub fn cleanup_events(&self, appservice_id: String) -> Result<()> {
        self.db
            .delete_all_requests_for(&OutgoingKind::Appservice(appservice_id))?;

        Ok(())
    }

    #[tracing::instrument(skip(events, kind))]
    async fn handle_events(
        kind: OutgoingKind,
        events: Vec<SendingEventType>,
    ) -> Result<OutgoingKind, (OutgoingKind, Error)> {
        match &kind {
            OutgoingKind::Appservice(id) => {
                let mut pdu_jsons = Vec::new();

                for event in &events {
                    match event {
                        SendingEventType::Pdu(pdu_id) => {
                            pdu_jsons.push(services().rooms.timeline
                                .get_pdu_from_id(pdu_id)
                                .map_err(|e| (kind.clone(), e))?
                                .ok_or_else(|| {
                                    (
                                        kind.clone(),
                                        Error::bad_database(
                                            "[Appservice] Event in servernameevent_data not found in db.",
                                        ),
                                    )
                                })?
                                .to_room_event())
                        }
                        SendingEventType::Edu(_) => {
                            // Appservices don't need EDUs (?)
                        }
                    }
                }

                let permit = services().sending.maximum_requests.acquire().await;

                let response = match appservice_server::send_request(
                    services()
                        .appservice
                        .get_registration(id)
                        .await
                        .ok_or_else(|| {
                            (
                                kind.clone(),
                                Error::bad_database(
                                    "[Appservice] Could not load registration from db.",
                                ),
                            )
                        })?,
                    appservice::event::push_events::v1::Request {
                        //TODO: ephemeral pushing
                        ephemeral: Vec::new(),
                        events: pdu_jsons,
                        txn_id: (&*general_purpose::URL_SAFE_NO_PAD.encode(calculate_hash(
                            &events
                                .iter()
                                .map(|e| match e {
                                    SendingEventType::Edu(b) | SendingEventType::Pdu(b) => &**b,
                                })
                                .collect::<Vec<_>>(),
                        )))
                            .into(),
                    },
                )
                .await
                {
                    Ok(_) => Ok(kind.clone()),
                    Err(e) => Err((kind.clone(), e)),
                };

                drop(permit);

                response
            }
            OutgoingKind::Push(userid, pushkey) => {
                let mut pdus = Vec::new();

                for event in &events {
                    match event {
                        SendingEventType::Pdu(pdu_id) => {
                            pdus.push(
                                services().rooms
                                    .timeline
                                    .get_pdu_from_id(pdu_id)
                                    .map_err(|e| (kind.clone(), e))?
                                    .ok_or_else(|| {
                                        (
                                            kind.clone(),
                                            Error::bad_database(
                                                "[Push] Event in servernamevent_data not found in db.",
                                            ),
                                        )
                                    })?,
                            );
                        }
                        SendingEventType::Edu(_) => {
                            // Push gateways don't need EDUs (?)
                        }
                    }
                }

                for pdu in pdus {
                    // Redacted events are not notification targets (we don't send push for them)
                    if let Some(unsigned) = &pdu.unsigned {
                        if let Ok(unsigned) =
                            serde_json::from_str::<serde_json::Value>(unsigned.get())
                        {
                            if unsigned.get("redacted_because").is_some() {
                                continue;
                            }
                        }
                    }

                    let pusher = match services()
                        .pusher
                        .get_pusher(userid, pushkey)
                        .map_err(|e| (OutgoingKind::Push(userid.clone(), pushkey.clone()), e))?
                    {
                        Some(pusher) => pusher,
                        None => continue,
                    };

                    let rules_for_user = services()
                        .account_data
                        .get(
                            None,
                            userid,
                            GlobalAccountDataEventType::PushRules.to_string().into(),
                        )
                        .unwrap_or_default()
                        .and_then(|event| serde_json::from_str::<PushRulesEvent>(event.get()).ok())
                        .map(|ev: PushRulesEvent| ev.content.global)
                        .unwrap_or_else(|| push::Ruleset::server_default(userid));

                    let unread: UInt = services()
                        .rooms
                        .user
                        .notification_count(userid, &pdu.room_id())
                        .map_err(|e| (kind.clone(), e))?
                        .try_into()
                        .expect("notification count can't go that high");

                    let permit = services().sending.maximum_requests.acquire().await;

                    let _response = services()
                        .pusher
                        .send_push_notice(userid, unread, &pusher, rules_for_user, &pdu)
                        .await
                        .map(|_response| kind.clone())
                        .map_err(|e| (kind.clone(), e));

                    drop(permit);
                }
                Ok(OutgoingKind::Push(userid.clone(), pushkey.clone()))
            }
            OutgoingKind::Normal(server) => {
                let mut edu_jsons = Vec::new();
                let mut pdu_jsons = Vec::new();

                for event in &events {
                    match event {
                        SendingEventType::Pdu(pdu_id) => {
                            // TODO: check room version and remove event_id if needed
                            let raw = PduEvent::convert_to_outgoing_federation_event(
                                services().rooms
                                    .timeline
                                    .get_pdu_json_from_id(pdu_id)
                                    .map_err(|e| (OutgoingKind::Normal(server.clone()), e))?
                                    .ok_or_else(|| {
                                        error!("event not found: {server} {pdu_id:?}");
                                        (
                                            OutgoingKind::Normal(server.clone()),
                                            Error::bad_database(
                                                "[Normal] Event in servernamevent_data not found in db.",
                                            ),
                                        )
                                    })?,
                            );
                            pdu_jsons.push(raw);
                        }
                        SendingEventType::Edu(edu) => {
                            if let Ok(raw) = serde_json::from_slice(edu) {
                                edu_jsons.push(raw);
                            }
                        }
                    }
                }

                let permit = services().sending.maximum_requests.acquire().await;

                let response = server_server::send_request(
                    server,
                    send_transaction_message::v1::Request {
                        origin: services().globals.server_name().to_owned(),
                        pdus: pdu_jsons,
                        edus: edu_jsons,
                        origin_server_ts: MilliSecondsSinceUnixEpoch::now(),
                        transaction_id: (&*general_purpose::URL_SAFE_NO_PAD.encode(
                            calculate_hash(
                                &events
                                    .iter()
                                    .map(|e| match e {
                                        SendingEventType::Edu(b) | SendingEventType::Pdu(b) => &**b,
                                    })
                                    .collect::<Vec<_>>(),
                            ),
                        ))
                            .into(),
                    },
                )
                .await
                .map(|response| {
                    for pdu in response.pdus {
                        if pdu.1.is_err() {
                            warn!("Failed to send to {}: {:?}", server, pdu);
                        }
                    }
                    kind.clone()
                })
                .map_err(|e| (kind, e));

                drop(permit);

                response
            }
        }
    }

    #[tracing::instrument(skip(self, destination, request))]
    pub async fn send_federation_request<T>(
        &self,
        destination: &ServerName,
        request: T,
    ) -> Result<T::IncomingResponse>
    where
        T: OutgoingRequest + Debug,
    {
        debug!("Waiting for permit");
        let permit = self.maximum_requests.acquire().await;
        debug!("Got permit");
        let response = tokio::time::timeout(
            Duration::from_secs(2 * 60),
            server_server::send_request(destination, request),
        )
        .await
        .map_err(|_| {
            warn!("Timeout waiting for server response of {destination}");
            Error::BadServerResponse("Timeout waiting for server response")
        })?;
        drop(permit);

        response
    }

    /// Sends a request to an appservice
    ///
    /// Only returns None if there is no url specified in the appservice registration file
    #[tracing::instrument(skip(self, registration, request))]
    pub async fn send_appservice_request<T>(
        &self,
        registration: Registration,
        request: T,
    ) -> Result<Option<T::IncomingResponse>>
    where
        T: OutgoingRequest + Debug,
    {
        let permit = self.maximum_requests.acquire().await;
        let response = appservice_server::send_request(registration, request).await;
        drop(permit);

        response
    }

    fn send_federation_edu<I: Iterator<Item = OwnedServerName>>(
        &self,
        servers: I,
        edu: Edu,
    ) -> Result<()> {
        let serialized_edu = serde_json::to_vec(&edu).expect("json can be serialized");

        let requests = servers
            .into_iter()
            .map(|server| {
                (
                    OutgoingKind::Normal(server),
                    SendingEventType::Edu(serialized_edu.clone()),
                )
            })
            .collect::<Vec<_>>();

        let keys = self.db.queue_requests(
            &requests
                .iter()
                .map(|(o, e)| (o, e.clone()))
                .collect::<Vec<_>>(),
        )?;

        for ((outgoing_kind, event), key) in requests.into_iter().zip(keys) {
            self.sender
                .send((outgoing_kind.to_owned(), event, key))
                .unwrap();
        }

        Ok(())
    }

    pub fn get_servers_in_room(&self, room_id: &ruma::RoomId) -> Result<HashSet<OwnedServerName>> {
        let mut servers: HashSet<OwnedServerName> = services()
            .rooms
            .state_cache
            .room_servers(room_id)
            .filter_map(|r| r.ok())
            .collect();

        servers.remove(services().globals.server_name());

        Ok(servers)
    }

    #[tracing::instrument(skip(self, user_id, room_id, event))]
    pub fn send_federation_receipt_edu(
        &self,
        user_id: &UserId,
        room_id: &ruma::RoomId,
        event: ruma::events::receipt::ReceiptEvent,
    ) -> Result<()> {
        let servers = self.get_servers_in_room(room_id)?;

        if servers.is_empty() {
            return Ok(());
        }

        let mut read = BTreeMap::new();
        let (event_id, mut receipt) = event
            .content
            .0
            .into_iter()
            .next()
            .expect("we only use one event per read receipt");
        let receipt = receipt
            .remove(&ReceiptType::Read)
            .expect("our read receipts always set this")
            .remove(user_id)
            .expect("our read receipts always have the user here");

        read.insert(
            user_id.to_owned(),
            ReceiptData {
                data: receipt,
                event_ids: vec![event_id],
            },
        );

        let receipt_map = ReceiptMap { read };

        let mut receipts = BTreeMap::new();
        receipts.insert(room_id.to_owned(), receipt_map);

        self.send_federation_edu(
            servers.into_iter(),
            Edu::Receipt(ReceiptContent { receipts }),
        )
    }
}
