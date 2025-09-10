use std::{
    borrow::Cow,
    collections::BTreeMap,
    convert::TryFrom,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

pub mod socket;

use bytesize::ByteSize;
use chrono::DateTime;
use clap::{Args, Parser};
use image::GenericImageView;
use regex::Regex;
use ruma::{
    api::appservice::Registration,
    events::{
        room::{
            canonical_alias::RoomCanonicalAliasEventContent,
            create::RoomCreateEventContent,
            guest_access::{GuestAccess, RoomGuestAccessEventContent},
            history_visibility::{HistoryVisibility, RoomHistoryVisibilityEventContent},
            join_rules::{JoinRule, RoomJoinRulesEventContent},
            member::{MembershipState, RoomMemberEventContent},
            message::{
                FileMessageEventContent, ImageMessageEventContent, MessageType,
                RoomMessageEventContent,
            },
            name::RoomNameEventContent,
            power_levels::RoomPowerLevelsEventContent,
            topic::RoomTopicEventContent,
            MediaSource,
        },
        TimelineEventType,
    },
    room_version_rules::RoomVersionRules,
    EventId, MilliSecondsSinceUnixEpoch, MxcUri, OwnedMxcUri, OwnedRoomAliasId, OwnedRoomId,
    OwnedServerName, RoomAliasId, RoomId, RoomVersionId, ServerName, UserId,
};
use serde_json::value::to_raw_value;
use tokio::sync::{mpsc, Mutex, RwLock};

use crate::{
    api::client_server::{self, leave_all_rooms, AUTO_GEN_PASSWORD_LENGTH},
    services,
    utils::{self, HtmlEscape},
    Error, PduEvent, Result,
};

use super::{
    media::{
        size, BlockedMediaInfo, FileInfo, MediaListItem, MediaQuery, MediaQueryFileInfo,
        MediaQueryThumbInfo, ServerNameOrUserId,
    },
    pdu::PduBuilder,
};
use command::{AdminCommand, DeactivatePurgeMediaArgs, ListMediaArgs};

pub mod command;





#[derive(Debug)]
pub enum AdminRoomEvent {
    ProcessMessage(String),
    SendMessage(RoomMessageEventContent),
}

pub struct Service {
    pub sender: mpsc::UnboundedSender<AdminRoomEvent>,
    receiver: Mutex<mpsc::UnboundedReceiver<AdminRoomEvent>>,
}

impl Service {
    pub fn build() -> Arc<Self> {
        let (sender, receiver) = mpsc::unbounded_channel();
        Arc::new(Self {
            sender,
            receiver: Mutex::new(receiver),
        })
    }

    pub fn start_handler(self: &Arc<Self>) {
        let self2 = Arc::clone(self);
        tokio::spawn(async move {
            self2.handler().await;
        });
    }

    async fn handler(&self) {
        let mut receiver = self.receiver.lock().await;
        // TODO: Use futures when we have long admin commands
        //let mut futures = FuturesUnordered::new();

        let conduit_user = services().globals.server_user();

        if let Ok(Some(conduit_room)) = services().admin.get_admin_room() {
            loop {
                tokio::select! {
                    Some(event) = receiver.recv() => {
                        let message_content = match event {
                            AdminRoomEvent::SendMessage(content) => content.into(),
                            AdminRoomEvent::ProcessMessage(room_message) => self.process_admin_message(room_message).await,
                        };

                        let mutex_state = Arc::clone(
                            services().globals
                                .roomid_mutex_state
                                .write()
                                .await
                                .entry(conduit_room.to_owned())
                                .or_default(),
                        );

                        let state_lock = mutex_state.lock().await;

                        services()
                            .rooms
                            .timeline
                            .build_and_append_pdu(
                                PduBuilder {
                                    event_type: TimelineEventType::RoomMessage,
                                    content: to_raw_value(&message_content)
                                        .expect("event is valid, we just created it"),
                                    unsigned: None,
                                    state_key: None,
                                    redacts: None,
                                    timestamp: None,
                                },
                                conduit_user,
                                &conduit_room,
                                &state_lock,
                            )
                            .await.unwrap();
                    }
                }
            }
        }
    }

    pub fn process_message(&self, room_message: String) {
        self.sender
            .send(AdminRoomEvent::ProcessMessage(room_message))
            .unwrap();
    }

    pub fn send_message(&self, message_content: RoomMessageEventContent) {
        self.sender
            .send(AdminRoomEvent::SendMessage(message_content))
            .unwrap();
    }

    // Parse and process a message from the admin room
    async fn process_admin_message(&self, room_message: String) -> MessageType {
        let mut lines = room_message.lines().filter(|l| !l.trim().is_empty());
        let command_line = lines.next().expect("each string has at least one line");
        let body: Vec<_> = lines.collect();

        let admin_command = match self.parse_admin_command(command_line) {
            Ok(command) => command,
            Err(error) => {
                let server_name = services().globals.server_name();
                let message = error.replace("server.name", server_name.as_str());
                let html_message = self.usage_to_html(&message, server_name);

                return RoomMessageEventContent::text_html(message, html_message).into();
            }
        };

        match self.process_admin_command(admin_command, body).await {
            Ok(reply_message) => reply_message,
            Err(error) => {
                let markdown_message = format!(
                    "Encountered an error while handling the command:\n\
                    ```\n{error}\n```",
                );
                let html_message = format!(
                    "Encountered an error while handling the command:\n\
                    <pre>\n{error}\n</pre>",
                );

                RoomMessageEventContent::text_html(markdown_message, html_message).into()
            }
        }
    }

    // Parse chat messages from the admin room into an AdminCommand object
    fn parse_admin_command(&self, command_line: &str) -> std::result::Result<AdminCommand, String> {
        let conduit_user = services().globals.server_user();
        let localpart = conduit_user.localpart();

        // List of possible prefixes
        let prefixes = [
            format!("{conduit_user}:"), // @conduit:server.name:
            format!("{conduit_user} "), // @conduit:server.name
            format!("{localpart}:"),    // conduit:
            format!("{localpart} "),    // conduit
        ];

        let mut processed_command = None;

        for prefix in &prefixes {
            if command_line.starts_with(prefix) {
                let command_part = command_line.strip_prefix(prefix).unwrap().trim_start();
                processed_command =
                    Some(format!("{} {}", format!("{conduit_user}:"), command_part));
                break;
            }
        }

        // Handle the case where it's just the username
        if command_line.trim() == conduit_user.as_str() || command_line.trim() == localpart {
            processed_command = Some(format!("{}: --help", conduit_user));
        }

        let final_command_line = processed_command.unwrap_or_else(|| command_line.to_string());

        // Note: argv[0] is `@conduit:servername:`, which is treated as the main command
        let mut argv = match shell_words::split(&final_command_line) {
            Ok(args) => args,
            Err(e) => return Err(format!("Failed to parse admin command: {e}")),
        };

        // Replace `help command` with `command --help`
        // Clap has a help subcommand, but it omits the long help description.
        if argv.len() > 1 && argv[1] == "help" {
            argv.remove(1);
            argv.push("--help".to_owned());
        }

        // Backwards compatibility with `register_appservice`-style commands
        if let Some(command) = argv.get_mut(1) {
            if command.contains('_') {
                *command = command.replace('_', "-");
            }
        }

        AdminCommand::try_parse_from(&argv).map_err(|error| error.to_string())
    }

    pub async fn process_admin_command(
        &self,
        command: AdminCommand,
        body: Vec<&str>,
    ) -> Result<MessageType> {
        let reply_message_content = match command {
            AdminCommand::RegisterAppservice => {
                if body.len() > 2 && body[0].trim() == "```" && body.last().unwrap().trim() == "```"
                {
                    let appservice_config = body[1..body.len() - 1].join("\n");
                    let parsed_config = serde_yaml::from_str::<Registration>(&appservice_config);
                    match parsed_config {
                        Ok(yaml) => match services().appservice.register_appservice(yaml).await {
                            Ok(id) => RoomMessageEventContent::text_plain(format!(
                                "Appservice registered with ID: {id}."
                            )),
                            Err(e) => RoomMessageEventContent::text_plain(format!(
                                "Failed to register appservice: {e}"
                            )),
                        },
                        Err(e) => RoomMessageEventContent::text_plain(format!(
                            "Could not parse appservice config: {e}"
                        )),
                    }
                } else {
                    RoomMessageEventContent::text_plain(
                        "Expected code block in command body. Add --help for details.",
                    )
                }
                .into()
            }
            AdminCommand::UnregisterAppservice {
                appservice_identifier,
            } => match services()
                .appservice
                .unregister_appservice(&appservice_identifier)
                .await
            {
                Ok(()) => RoomMessageEventContent::text_plain("Appservice unregistered."),
                Err(e) => RoomMessageEventContent::text_plain(format!(
                    "Failed to unregister appservice: {e}"
                )),
            }
            .into(),
            AdminCommand::ListAppservices => {
                let appservices = services().appservice.iter_ids().await;
                let output = format!(
                    "Appservices ({}): {}",
                    appservices.len(),
                    appservices.join(", ")
                );
                RoomMessageEventContent::text_plain(output).into()
            }
            AdminCommand::RoomInfo { room_id_or_alias } => {
                let room_id = if room_id_or_alias.starts_with('!') {
                    RoomId::parse(&room_id_or_alias)
                        .map_err(|_| Error::AdminCommand("Invalid room ID"))?
                } else if room_id_or_alias.starts_with('#') {
                    let alias = RoomAliasId::parse(&room_id_or_alias)
                        .map_err(|_| Error::AdminCommand("Invalid room alias"))?;
                    services()
                        .rooms
                        .alias
                        .resolve_local_alias(&alias)?
                        .ok_or_else(|| Error::AdminCommand("Room alias not found."))?
                } else {
                    return Err(Error::AdminCommand(
                        "Invalid room ID or alias. Must start with '!' or '#'",
                    ));
                };

                if !services().rooms.metadata.exists(&room_id)? {
                    return Ok(RoomMessageEventContent::text_plain("Room not found.").into());
                }

                let shortstatehash = services()
                    .rooms
                    .state
                    .get_room_shortstatehash(&room_id)?
                    .ok_or_else(|| Error::bad_database("Room has no state"))?;

                let mut message = format!("Room Information for: {}\n", &room_id_or_alias);
                message.push_str("---------------------------------\n");
                message.push_str(&format!("Room ID: {}\n", room_id));

                if let Some(event) = services().rooms.state_accessor.state_get(
                    shortstatehash,
                    &ruma::events::StateEventType::RoomCanonicalAlias,
                    "",
                )? {
                    let content =
                        serde_json::from_str::<RoomCanonicalAliasEventContent>(event.content.get())
                            .map_err(|_| Error::bad_database("Invalid canonical alias event"))?;
                    if let Some(alias) = content.alias {
                        message.push_str(&format!("Canonical Alias: {}\n", alias));
                    }
                    if !content.alt_aliases.is_empty() {
                        message.push_str(&format!("Aliases: {:?}\n", content.alt_aliases));
                    }
                }

                if let Some(event) = services().rooms.state_accessor.state_get(
                    shortstatehash,
                    &ruma::events::StateEventType::RoomName,
                    "",
                )? {
                    let content = serde_json::from_str::<RoomNameEventContent>(event.content.get())
                        .map_err(|_| Error::bad_database("Invalid room name event"))?;
                    message.push_str(&format!("Name: {}\n", content.name));
                }

                if let Some(event) = services().rooms.state_accessor.state_get(
                    shortstatehash,
                    &ruma::events::StateEventType::RoomTopic,
                    "",
                )? {
                    let content =
                        serde_json::from_str::<RoomTopicEventContent>(event.content.get())
                            .map_err(|_| Error::bad_database("Invalid room topic event"))?;
                    message.push_str(&format!("Topic: {}\n", content.topic));
                }

                if let Some(event) = services().rooms.state_accessor.state_get(
                    shortstatehash,
                    &ruma::events::StateEventType::RoomAvatar,
                    "",
                )? {
                    let content = serde_json::from_str::<
                        ruma::events::room::avatar::RoomAvatarEventContent,
                    >(event.content.get())
                    .map_err(|_| Error::bad_database("Invalid room avatar event"))?;
                    if let Some(url) = content.url {
                        message.push_str(&format!("Avatar URL: {}\n", url));
                    }
                }

                message.push_str("---------------------------------\n");
                message.push_str("State:\n");

                if let Some(event) = services().rooms.state_accessor.state_get(
                    shortstatehash,
                    &ruma::events::StateEventType::RoomJoinRules,
                    "",
                )? {
                    let content =
                        serde_json::from_str::<RoomJoinRulesEventContent>(event.content.get())
                            .map_err(|_| Error::bad_database("Invalid join rules event"))?;
                    let join_rule_str = match content.join_rule {
                        JoinRule::Public => "Public",
                        JoinRule::Knock => "Knock",
                        JoinRule::Invite => "Invite",
                        JoinRule::Private => "Private",
                        JoinRule::Restricted(_) => "Restricted",
                        JoinRule::KnockRestricted(_) => "KnockRestricted",
                        _ => "Custom",
                    };
                    message.push_str(&format!("- Join Rule: {}\n", join_rule_str));
                }

                if let Some(event) = services().rooms.state_accessor.state_get(
                    shortstatehash,
                    &ruma::events::StateEventType::RoomHistoryVisibility,
                    "",
                )? {
                    let content = serde_json::from_str::<RoomHistoryVisibilityEventContent>(
                        event.content.get(),
                    )
                    .map_err(|_| Error::bad_database("Invalid history visibility event"))?;
                    let history_visibility_str = match content.history_visibility {
                        HistoryVisibility::Invited => "Invited",
                        HistoryVisibility::Joined => "Joined",
                        HistoryVisibility::Shared => "Shared",
                        HistoryVisibility::WorldReadable => "WorldReadable",
                        _ => "Custom",
                    };
                    message.push_str(&format!("- Visibility: {}\n", history_visibility_str));
                }

                if let Some(_event) = services().rooms.state_accessor.state_get(
                    shortstatehash,
                    &ruma::events::StateEventType::RoomEncryption,
                    "",
                )? {
                    message.push_str(&format!("- Encrypted: {}\n", true));
                } else {
                    message.push_str(&format!("- Encrypted: {}\n", false));
                }

                message.push_str("---------------------------------\n");

                let members: Vec<ruma::OwnedUserId> = services()
                    .rooms
                    .state_cache
                    .room_members(&room_id)
                    .filter_map(Result::ok)
                    .collect();
                let power_levels = services().rooms.state_accessor.power_levels(&room_id)?;

                message.push_str(&format!("Members (Count: {}):\n", members.len()));
                for user_id in members {
                    let power_level = power_levels
                        .users
                        .get(&user_id)
                        .map_or(power_levels.users_default, |p| *p);
                    message.push_str(&format!("- {} (Power Level: {})\n", user_id, power_level));
                }

                RoomMessageEventContent::text_plain(message).into()
            }
            AdminCommand::ListRooms => {
                let room_ids = services().rooms.metadata.iter_ids();
                let output = format!(
                    "Rooms:\n{}",
                    room_ids
                        .filter_map(|r| r.ok())
                        .map(|id| id.to_string()
                            + "\tMembers: "
                            + &services()
                                .rooms
                                .state_cache
                                .room_joined_count(&id)
                                .ok()
                                .flatten()
                                .unwrap_or(0)
                                .to_string())
                        .collect::<Vec<_>>()
                        .join("\n")
                );
                RoomMessageEventContent::text_plain(output).into()
            }
            AdminCommand::ListLocalUsers => match services().users.list_local_users() {
                Ok(users) => {
                    let mut msg: String = format!("Found {} local user account(s):\n", users.len());
                    msg += &users.join("\n");
                    RoomMessageEventContent::text_plain(&msg)
                }
                Err(e) => RoomMessageEventContent::text_plain(e.to_string()),
            }
            .into(),
            AdminCommand::IncomingFederation => {
                let map = services().globals.roomid_federationhandletime.read().await;
                let mut msg: String = format!("Handling {} incoming pdus:\n", map.len());

                for (r, (e, i)) in map.iter() {
                    let elapsed = i.elapsed();
                    msg += &format!(
                        "{} {}: {}m{}s\n",
                        r,
                        e,
                        elapsed.as_secs() / 60,
                        elapsed.as_secs() % 60
                    );
                }
                RoomMessageEventContent::text_plain(&msg).into()
            }
            AdminCommand::GetAuthChain { event_id } => {
                let event_id = Arc::<EventId>::from(event_id);
                if let Some(event) = services().rooms.timeline.get_pdu_json(&event_id)? {
                    let room_id_str = event
                        .get("room_id")
                        .and_then(|val| val.as_str())
                        .ok_or_else(|| Error::bad_database("Invalid event in database"))?;

                    let room_id = <&RoomId>::try_from(room_id_str).map_err(|_| {
                        Error::bad_database("Invalid room id field in event in database")
                    })?;
                    let start = Instant::now();
                    let count = services()
                        .rooms
                        .auth_chain
                        .get_auth_chain(room_id, vec![event_id])
                        .await?
                        .count();
                    let elapsed = start.elapsed();
                    RoomMessageEventContent::text_plain(format!(
                        "Loaded auth chain with length {count} in {elapsed:?}"
                    ))
                } else {
                    RoomMessageEventContent::text_plain("Event not found.")
                }
                .into()
            }
            AdminCommand::ParsePdu => {
                if body.len() > 2 && body[0].trim() == "```" && body.last().unwrap().trim() == "```"
                {
                    let string = body[1..body.len() - 1].join("\n");
                    match serde_json::from_str(&string) {
                        Ok(value) => {
                            match ruma::signatures::reference_hash(&value, &RoomVersionRules::V11) {
                                Ok(hash) => {
                                    let event_id = EventId::parse(format!("${hash}"));

                                    match serde_json::from_value::<PduEvent>(
                                        serde_json::to_value(value).expect("value is json"),
                                    ) {
                                        Ok(pdu) => RoomMessageEventContent::text_plain(format!(
                                            "EventId: {event_id:?}\n{pdu:#?}"
                                        )),
                                        Err(e) => RoomMessageEventContent::text_plain(format!(
                                            "EventId: {event_id:?}\nCould not parse event: {e}"
                                        )),
                                    }
                                }
                                Err(e) => RoomMessageEventContent::text_plain(format!(
                                    "Could not parse PDU JSON: {e:?}"
                                )),
                            }
                        }
                        Err(e) => RoomMessageEventContent::text_plain(format!(
                            "Invalid json in command body: {e}"
                        )),
                    }
                } else {
                    RoomMessageEventContent::text_plain("Expected code block in command body.")
                }
                .into()
            }
            AdminCommand::GetPdu { event_id } => {
                let mut outlier = false;
                let mut pdu_json = services()
                    .rooms
                    .timeline
                    .get_non_outlier_pdu_json(&event_id)?;
                if pdu_json.is_none() {
                    outlier = true;
                    pdu_json = services().rooms.timeline.get_pdu_json(&event_id)?;
                }
                match pdu_json {
                    Some(json) => {
                        let json_text = serde_json::to_string_pretty(&json)
                            .expect("canonical json is valid json");
                        RoomMessageEventContent::text_html(
                            format!(
                                "{}\n```json\n{}\n```",
                                if outlier {
                                    "PDU is outlier"
                                } else {
                                    "PDU was accepted"
                                },
                                json_text
                            ),
                            format!(
                                "<p>{}</p>\n<pre><code class=\"language-json\">{}\n</code></pre>\n",
                                if outlier {
                                    "PDU is outlier"
                                } else {
                                    "PDU was accepted"
                                },
                                HtmlEscape(&json_text)
                            ),
                        )
                    }
                    None => RoomMessageEventContent::text_plain("PDU not found."),
                }
                .into()
            }
            AdminCommand::MemoryUsage => {
                let response1 = services().memory_usage().await;
                let response2 = services().globals.db.memory_usage();

                RoomMessageEventContent::text_plain(format!(
                    "Services:\n{response1}\n\nDatabase:\n{response2}"
                ))
                .into()
            }
            AdminCommand::ClearDatabaseCaches { amount } => {
                services().globals.db.clear_caches(amount);

                RoomMessageEventContent::text_plain("Done.").into()
            }
            AdminCommand::ClearServiceCaches { amount } => {
                services().clear_caches(amount).await;

                RoomMessageEventContent::text_plain("Done.").into()
            }
            AdminCommand::ShowConfig => {
                // Construct and send the response
                RoomMessageEventContent::text_plain(format!("{}", services().globals.config)).into()
            }
            AdminCommand::ResetPassword { username } => {
                let user_id = match UserId::parse_with_server_name(
                    username.as_str().to_lowercase(),
                    services().globals.server_name(),
                ) {
                    Ok(id) => id,
                    Err(e) => {
                        return Ok(RoomMessageEventContent::text_plain(format!(
                            "The supplied username is not a valid username: {e}"
                        ))
                        .into())
                    }
                };

                // Checks if user is local
                if user_id.server_name() != services().globals.server_name() {
                    return Ok(RoomMessageEventContent::text_plain(
                        "The specified user is not from this server!",
                    )
                    .into());
                };

                // Check if the specified user is valid
                if !services().users.exists(&user_id)?
                    || user_id
                        == UserId::parse_with_server_name(
                            "conduit",
                            services().globals.server_name(),
                        )
                        .expect("conduit user exists")
                {
                    return Ok(RoomMessageEventContent::text_plain(
                        "The specified user does not exist!",
                    )
                    .into());
                }

                let new_password = utils::random_string(AUTO_GEN_PASSWORD_LENGTH);

                match services()
                    .users
                    .set_password(&user_id, Some(new_password.as_str()))
                {
                    Ok(()) => RoomMessageEventContent::text_plain(format!(
                        "Successfully reset the password for user {user_id}: {new_password}"
                    )),
                    Err(e) => RoomMessageEventContent::text_plain(format!(
                        "Couldn't reset the password for user {user_id}: {e}"
                    )),
                }
                .into()
            }
            AdminCommand::CreateUser { username, password } => {
                let password =
                    password.unwrap_or_else(|| utils::random_string(AUTO_GEN_PASSWORD_LENGTH));
                // Validate user id
                let user_id = match UserId::parse_with_server_name(
                    username.as_str().to_lowercase(),
                    services().globals.server_name(),
                ) {
                    Ok(id) => id,
                    Err(e) => {
                        return Ok(RoomMessageEventContent::text_plain(format!(
                            "The supplied username is not a valid username: {e}"
                        ))
                        .into())
                    }
                };

                // Checks if user is local
                if user_id.server_name() != services().globals.server_name() {
                    return Ok(RoomMessageEventContent::text_plain(
                        "The specified user is not from this server!",
                    )
                    .into());
                };

                if user_id.is_historical() {
                    return Ok(RoomMessageEventContent::text_plain(format!(
                        "Userid {user_id} is not allowed due to historical"
                    ))
                    .into());
                }
                if services().users.exists(&user_id)? {
                    return Ok(RoomMessageEventContent::text_plain(format!(
                        "Userid {user_id} already exists"
                    ))
                    .into());
                }
                // Create user
                services().users.create(&user_id, Some(password.as_str()))?;

                // Default to pretty displayname
                let mut displayname = user_id.localpart().to_owned();

                // If enabled append lightning bolt to display name (default true)
                if services().globals.enable_lightning_bolt() {
                    displayname.push_str(" ⚡️");
                }

                services()
                    .users
                    .set_displayname(&user_id, Some(displayname))?;

                // Initial account data
                services().account_data.update(
                    None,
                    &user_id,
                    ruma::events::GlobalAccountDataEventType::PushRules
                        .to_string()
                        .into(),
                    &serde_json::to_value(ruma::events::push_rules::PushRulesEvent {
                        content: ruma::events::push_rules::PushRulesEventContent {
                            global: ruma::push::Ruleset::server_default(&user_id),
                        },
                    })
                    .expect("to json value always works"),
                )?;

                // we dont add a device since we're not the user, just the creator

                // Inhibit login does not work for guests
                RoomMessageEventContent::text_plain(format!(
                    "Created user with user_id: {user_id} and password: {password}"
                ))
                .into()
            }
            AdminCommand::AllowRegistration { status } => if let Some(status) = status {
                services().globals.set_registration(status).await;
                RoomMessageEventContent::text_plain(if status {
                    "Registration is now enabled"
                } else {
                    "Registration is now disabled"
                })
            } else {
                RoomMessageEventContent::text_plain(
                    if services().globals.allow_registration().await {
                        "Registration is currently enabled"
                    } else {
                        "Registration is currently disabled"
                    },
                )
            }
            .into(),
            AdminCommand::DisableRoom { room_id } => {
                services().rooms.metadata.disable_room(&room_id, true)?;
                RoomMessageEventContent::text_plain("Room disabled.").into()
            }
            AdminCommand::EnableRoom { room_id } => {
                services().rooms.metadata.disable_room(&room_id, false)?;
                RoomMessageEventContent::text_plain("Room enabled.").into()
            }
            AdminCommand::DeactivateUser {
                leave_rooms,
                user_id,
                purge_media,
            } => {
                let user_id = Arc::<UserId>::from(user_id);
                if !services().users.exists(&user_id)? {
                    RoomMessageEventContent::text_plain(format!(
                        "User {user_id} doesn't exist on this server"
                    ))
                } else if user_id.server_name() != services().globals.server_name() {
                    RoomMessageEventContent::text_plain(format!(
                        "User {user_id} is not from this server"
                    ))
                } else {
                    RoomMessageEventContent::text_plain(format!(
                        "Making {user_id} leave all rooms before deactivation..."
                    ));

                    services().users.deactivate_account(&user_id)?;

                    if leave_rooms {
                        leave_all_rooms(&user_id).await?;
                    }

                    let failed_purged_media = if purge_media.purge_media {
                        let after = purge_media
                            .media_from_last
                            .map(unix_secs_from_duration)
                            .transpose()?;

                        services()
                            .media
                            .purge_from_user(&user_id, purge_media.force_filehash, after)
                            .await
                            .len()
                    } else {
                        0
                    };

                    if failed_purged_media == 0 {
                        RoomMessageEventContent::text_plain(format!(
                            "User {user_id} has been deactivated"
                        ))
                    } else {
                        RoomMessageEventContent ::text_plain(format!(
                        "User {user_id} has been deactivated, but {failed_purged_media} media failed to be purged, check the logs for more details"
                    ))
                    }
                }.into()
            }
            AdminCommand::DeactivateAll {
                leave_rooms,
                force,
                purge_media,
            } => {
                if body.len() > 2 && body[0].trim() == "```" && body.last().unwrap().trim() == "```"
                {
                    let mut user_ids = match userids_from_body(&body)? {
                        Ok(v) => v,
                        Err(message) => return Ok(message),
                    };

                    let mut deactivation_count = 0;
                    let mut admins = Vec::new();

                    if !force {
                        user_ids.retain(|&user_id| match services().users.is_admin(user_id) {
                            Ok(is_admin) => match is_admin {
                                true => {
                                    admins.push(user_id.localpart());
                                    false
                                }
                                false => true,
                            },
                            Err(_) => false,
                        })
                    }

                    for &user_id in &user_ids {
                        if services().users.deactivate_account(user_id).is_ok() {
                            deactivation_count += 1
                        }
                    }

                    if leave_rooms {
                        for &user_id in &user_ids {
                            let _ = leave_all_rooms(user_id).await;
                        }
                    }

                    let mut failed_count = 0;

                    if purge_media.purge_media {
                        let after = purge_media
                            .media_from_last
                            .map(unix_secs_from_duration)
                            .transpose()?;

                        for user_id in user_ids {
                            failed_count += services()
                                .media
                                .purge_from_user(user_id, purge_media.force_filehash, after)
                                .await
                                .len();
                        }
                    }

                    let mut message = format!("Deactivated {deactivation_count} accounts.");
                    if !admins.is_empty() {
                        message.push_str(&format!(
                        "\nSkipped admin accounts: {:?}. Use --force to deactivate admin accounts",
                        admins.join(", ")
                    ));
                    }
                    if failed_count != 0 {
                        message.push_str(&format!(
                            "\nFailed to delete {failed_count} media, check logs for more details"
                        ))
                    }

                    RoomMessageEventContent::text_plain(message)
                } else {
                    RoomMessageEventContent::text_plain(
                        "Expected code block in command body. Add --help for details.",
                    )
                }
                .into()
            }
            AdminCommand::QueryMedia { mxc } => {
                let Ok((server_name, media_id)) = mxc.parts() else {
                    return Ok(RoomMessageEventContent::text_plain("Invalid media MXC").into());
                };

                let MediaQuery {
                    is_blocked,
                    source_file,
                    thumbnails,
                } = services().media.query(server_name, media_id)?;
                let mut message = format!("Is blocked Media ID: {is_blocked}");

                if let Some(MediaQueryFileInfo {
                    uploader_localpart,
                    sha256_hex,
                    filename,
                    content_type,
                    unauthenticated_access_permitted,
                    is_blocked_via_filehash,
                    file_info: time_info,
                }) = source_file
                {
                    message.push_str("\n\nInformation on full (non-thumbnail) file:\n");

                    if let Some(FileInfo {
                        creation,
                        last_access,
                        size,
                    }) = time_info
                    {
                        message.push_str(&format!("\nIs stored: true\nCreated at: {}\nLast accessed at: {}\nSize of file: {}",
                            DateTime::from_timestamp(creation.try_into().unwrap_or(i64::MAX),0).expect("Timestamp is within range"),
                            DateTime::from_timestamp(last_access.try_into().unwrap_or(i64::MAX),0).expect("Timestamp is within range"),
                            ByteSize::b(size).display().si()
                        ));
                    } else {
                        message.push_str("\nIs stored: false");
                    }

                    message.push_str(&format!("\nIs accessible via unauthenticated media endpoints: {unauthenticated_access_permitted}"));
                    message.push_str(&format!("\nSHA256 hash of file: {sha256_hex}"));
                    message.push_str(&format!("\nIs blocked due to sharing SHA256 hash with blocked media: {is_blocked_via_filehash}"));

                    if let Some(localpart) = uploader_localpart {
                        message.push_str(&format!("\nUploader: @{localpart}:{server_name}"))
                    }
                    if let Some(filename) = filename {
                        message.push_str(&format!("\nFilename: {filename}"))
                    }
                    if let Some(content_type) = content_type {
                        message.push_str(&format!("\nContent-type: {content_type}"))
                    }
                }

                if !thumbnails.is_empty() {
                    message.push_str("\n\nInformation on thumbnails of media:");
                }

                for MediaQueryThumbInfo {
                    width,
                    height,
                    sha256_hex,
                    filename,
                    content_type,
                    unauthenticated_access_permitted,
                    is_blocked_via_filehash,
                    file_info: time_info,
                } in thumbnails
                {
                    message.push_str(&format!("\n\nDimensions: {width}x{height}"));
                    if let Some(FileInfo {
                        creation,
                        last_access,
                        size,
                    }) = time_info
                    {
                        message.push_str(&format!("\nIs stored: true\nCreated at: {}\nLast accessed at: {}\nSize of file: {}",
                            DateTime::from_timestamp(creation.try_into().unwrap_or(i64::MAX),0).expect("Timestamp is within range"),
                            DateTime::from_timestamp(last_access.try_into().unwrap_or(i64::MAX),0).expect("Timestamp is within range"),
                            ByteSize::b(size).display().si()
                        ));
                    } else {
                        message.push_str("\nIs stored: false");
                    }

                    message.push_str(&format!("\nIs accessible via unauthenticated media endpoints: {unauthenticated_access_permitted}"));
                    message.push_str(&format!("\nSHA256 hash of file: {sha256_hex}"));
                    message.push_str(&format!("\nIs blocked due to sharing SHA256 hash with blocked media: {is_blocked_via_filehash}"));

                    if let Some(filename) = filename {
                        message.push_str(&format!("\nFilename: {filename}"))
                    }
                    if let Some(content_type) = content_type {
                        message.push_str(&format!("\nContent-type: {content_type}"))
                    }
                }

                RoomMessageEventContent::text_plain(message).into()
            }
            AdminCommand::ShowMedia { mxc } => {
                let Ok((server_name, media_id)) = mxc.parts() else {
                    return Ok(RoomMessageEventContent::text_plain("Invalid media MXC").into());
                };

                // TODO: Bypass blocking once MSC3911 is implemented (linking media to events)
                let ruma::api::client::authenticated_media::get_content::v1::Response {
                    file,
                    content_type,
                    content_disposition,
                } = client_server::media::get_content(server_name, media_id.to_owned(), true, true)
                    .await?;

                if let Ok(image) = image::load_from_memory(&file) {
                    let filename = content_disposition.and_then(|cd| cd.filename);
                    let (width, height) = image.dimensions();

                    MessageType::Image(ImageMessageEventContent {
                        body: filename.clone().unwrap_or_default(),
                        formatted: None,
                        filename,
                        source: MediaSource::Plain(OwnedMxcUri::from(mxc.to_owned())),
                        info: Some(Box::new(ruma::events::room::ImageInfo {
                            height: Some(height.into()),
                            width: Some(width.into()),
                            mimetype: content_type,
                            size: size(&file)?.try_into().ok(),
                            thumbnail_info: None,
                            thumbnail_source: None,
                            blurhash: None,
                            thumbhash: None,
                        })),
                    })
                } else {
                    let filename = content_disposition.and_then(|cd| cd.filename);

                    MessageType::File(FileMessageEventContent {
                        body: filename.clone().unwrap_or_default(),
                        formatted: None,
                        filename,
                        source: MediaSource::Plain(OwnedMxcUri::from(mxc.to_owned())),
                        info: Some(Box::new(ruma::events::room::message::FileInfo {
                            mimetype: content_type,
                            size: size(&file)?.try_into().ok(),
                            thumbnail_info: None,
                            thumbnail_source: None,
                        })),
                    })
                }
            }
            AdminCommand::ListMedia {
                user_server_filter: ListMediaArgs { user, server },
                include_thumbnails,
                content_type,
                uploaded_before,
                uploaded_after,
            } => {
                let mut markdown_message = String::from(
                    "| MXC URI | Dimensions (if thumbnail) | Created/Downloaded at | Uploader | Content-Type | Filename | Size |\n| --- | --- | --- | --- | --- | --- | --- |",
                );
                let mut html_message = String::from(
                    r#"<table><thead><tr><th scope="col">MXC URI</th><th scope="col">Dimensions (if thumbnail)</th><th scope="col">Created/Downloaded at</th><th scope="col">Uploader</th><th scope="col">Content-Type</th><th scope="col">Filename</th><th scope="col">Size</th></tr></thead><tbody>"#,
                );

                for MediaListItem {
                    server_name,
                    media_id,
                    uploader_localpart,
                    content_type,
                    filename,
                    dimensions,
                    size,
                    creation,
                } in services().media.list(
                    user.map(ServerNameOrUserId::UserId)
                        .or_else(|| server.map(ServerNameOrUserId::ServerName)),
                    include_thumbnails,
                    content_type.as_deref(),
                    uploaded_before
                        .map(|ts| ts.duration_since(UNIX_EPOCH))
                        .transpose()
                        .map_err(|_| Error::AdminCommand("Timestamp must be after unix epoch"))?
                        .as_ref()
                        .map(Duration::as_secs),
                    uploaded_after
                        .map(|ts| ts.duration_since(UNIX_EPOCH))
                        .transpose()
                        .map_err(|_| Error::AdminCommand("Timestamp must be after unix epoch"))?
                        .as_ref()
                        .map(Duration::as_secs),
                )? {
                    let user_id = uploader_localpart
                        .map(|localpart| format!("@{localpart}:{server_name}"))
                        .unwrap_or_default();
                    let content_type = content_type.unwrap_or_default();
                    let filename = filename.unwrap_or_default();
                    let dimensions = dimensions
                        .map(|(w, h)| format!("{w}x{h}"))
                        .unwrap_or_default();
                    let size = ByteSize::b(size).display().si();
                    let creation =
                        DateTime::from_timestamp(creation.try_into().unwrap_or(i64::MAX), 0)
                            .expect("Timestamp is within range");

                    markdown_message
                        .push_str(&format!("\n| mxc://{server_name}/{media_id} | {dimensions} | {creation} | {user_id} | {content_type} | {filename} | {size} |"));

                    html_message.push_str(&format!(
                        "<tr><td>mxc://{server_name}/{media_id}</td><td>{dimensions}</td><td>{creation}</td><td>{user_id}</td><td>{content_type}</td><td>{filename}</td><td>{size}</td></tr>"
                    ))
                }

                html_message.push_str("</tbody></table>");

                RoomMessageEventContent::text_html(markdown_message, html_message).into()
            }
            AdminCommand::PurgeMedia => match media_from_body(body) {
                Ok(media) => {
                    let failed_count = services().media.purge(&media, true).await.len();

                    if failed_count == 0 {
                        RoomMessageEventContent::text_plain("Successfully purged media")
                    } else {
                        RoomMessageEventContent::text_plain(format!(
                            "Failed to delete {failed_count} media, check logs for more details"
                        ))
                    }
                    .into()
                }
                Err(message) => message,
            },
            AdminCommand::PurgeMediaFromUsers {
                from_last,
                force_filehash,
            } => {
                let after = from_last.map(unix_secs_from_duration).transpose()?;

                if body.len() > 2 && body[0].trim() == "```" && body.last().unwrap().trim() == "```"
                {
                    let user_ids = match userids_from_body(&body)? {
                        Ok(v) => v,
                        Err(message) => return Ok(message),
                    };

                    let mut failed_count = 0;

                    for user_id in user_ids {
                        failed_count += services()
                            .media
                            .purge_from_user(user_id, force_filehash, after)
                            .await
                            .len();
                    }

                    if failed_count == 0 {
                        RoomMessageEventContent::text_plain("Successfully purged media")
                    } else {
                        RoomMessageEventContent::text_plain(format!(
                            "Failed to purge {failed_count} media, check logs for more details"
                        ))
                    }
                } else {
                    RoomMessageEventContent::text_plain(
                        "Expected code block in command body. Add --help for details.",
                    )
                }
                .into()
            }
            AdminCommand::PurgeMediaFromServer {
                server_id: server_name,
                from_last,
                force_filehash,
            } => {
                if server_name == services().globals.server_name() {
                    return Err(Error::AdminCommand(
                        "Cannot purge all media from your own homeserver",
                    ));
                }

                let after = from_last.map(unix_secs_from_duration).transpose()?;

                let failed_count = services()
                    .media
                    .purge_from_server(&server_name, force_filehash, after)
                    .await
                    .len();

                if failed_count == 0 {
                    RoomMessageEventContent::text_plain(format!(
                        "Media from {server_name} has successfully been purged"
                    ))
                } else {
                    RoomMessageEventContent::text_plain(format!(
                        "Failed to purge {failed_count} media, check logs for more details"
                    ))
                }
                .into()
            }
            AdminCommand::BlockMedia { and_purge, reason } => match media_from_body(body) {
                Ok(media) => {
                    let failed_count = services().media.block(&media, reason).len();
                    let failed_purge_count = if and_purge {
                        services().media.purge(&media, true).await.len()
                    } else {
                        0
                    };

                    match (failed_count == 0, failed_purge_count == 0) {
                        (true, true) => RoomMessageEventContent::text_plain("Successfully blocked media"),
                        (false, true) => RoomMessageEventContent::text_plain(format!(
                            "Failed to block {failed_count} media, check logs for more details"
                        )),
                        (true, false ) => RoomMessageEventContent::text_plain(format!(
                            "Failed to purge {failed_purge_count} media, check logs for more details"
                        )),
                        (false, false) => RoomMessageEventContent::text_plain(format!(
                            "Failed to block {failed_count}, and purge {failed_purge_count} media, check logs for more details"
                        ))
                    }.into()
                }
                Err(message) => message,
            },
            AdminCommand::BlockMediaFromUsers { from_last, reason } => {
                let after = from_last.map(unix_secs_from_duration).transpose()?;

                if body.len() > 2 && body[0].trim() == "```" && body.last().unwrap().trim() == "```"
                {
                    let user_ids = match userids_from_body(&body)? {
                        Ok(v) => v,
                        Err(message) => return Ok(message),
                    };

                    let mut failed_count = 0;

                    for user_id in user_ids {
                        let reason = reason.as_ref().map_or_else(
                            || Cow::Owned(format!("uploaded by {user_id}")),
                            Cow::Borrowed,
                        );

                        failed_count += services()
                            .media
                            .block_from_user(user_id, &reason, after)
                            .len();
                    }

                    if failed_count == 0 {
                        RoomMessageEventContent::text_plain("Successfully blocked media")
                    } else {
                        RoomMessageEventContent::text_plain(format!(
                            "Failed to block {failed_count} media, check logs for more details"
                        ))
                    }
                } else {
                    RoomMessageEventContent::text_plain(
                        "Expected code block in command body. Add --help for details.",
                    )
                }
                .into()
            }
            AdminCommand::ListBlockedMedia => {
                let mut markdown_message = String::from(
                    "| SHA256 hash | MXC URI | Time Blocked | Reason |\n| --- | --- | --- | --- |",
                );
                let mut html_message = String::from(
                    r#"<table><thead><tr><th scope="col">SHA256 hash</th><th scope="col">MXC URI</th><th scope="col">Time Blocked</th><th scope="col">Reason</th></tr></thead><tbody>"#,
                );

                for media in services().media.list_blocked() {
                    let Ok(BlockedMediaInfo {
                        server_name,
                        media_id,
                        unix_secs,
                        reason,
                        sha256_hex,
                    }) = media
                    else {
                        continue;
                    };

                    let sha256_hex = sha256_hex.unwrap_or_default();
                    let reason = reason.unwrap_or_default();

                    let time = i64::try_from(unix_secs)
                        .map(|unix_secs| DateTime::from_timestamp(unix_secs, 0))
                        .ok()
                        .flatten()
                        .expect("Time is valid");

                    markdown_message.push_str(&format!(
                        "\n| {sha256_hex} | mxc://{server_name}/{media_id} | {time} | {reason} |"
                    ));

                    html_message.push_str(&format!(
                        "<tr><td>{sha256_hex}</td><td>mxc://{server_name}/{media_id}</td><td>{time}</td><td>{reason}</td></tr>",
                    ))
                }

                html_message.push_str("</tbody></table>");

                RoomMessageEventContent::text_html(markdown_message, html_message).into()
            }
            AdminCommand::UnblockMedia => media_from_body(body).map_or_else(
                |message| message,
                |media| {
                    let failed_count = services().media.unblock(&media).len();

                    if failed_count == 0 {
                        RoomMessageEventContent::text_plain("Successfully unblocked media")
                    } else {
                        RoomMessageEventContent::text_plain(format!(
                            "Failed to unblock {failed_count} media, check logs for more details"
                        ))
                    }
                    .into()
                },
            ),
            AdminCommand::SignJson => {
                if body.len() > 2 && body[0].trim() == "```" && body.last().unwrap().trim() == "```"
                {
                    let string = body[1..body.len() - 1].join("\n");
                    match serde_json::from_str(&string) {
                        Ok(mut value) => {
                            ruma::signatures::sign_json(
                                services().globals.server_name().as_str(),
                                services().globals.keypair(),
                                &mut value,
                            )
                            .expect("our request json is what ruma expects");
                            let json_text = serde_json::to_string_pretty(&value)
                                .expect("canonical json is valid json");
                            RoomMessageEventContent::text_plain(json_text)
                        }
                        Err(e) => RoomMessageEventContent::text_plain(format!("Invalid json: {e}")),
                    }
                } else {
                    RoomMessageEventContent::text_plain(
                        "Expected code block in command body. Add --help for details.",
                    )
                }
                .into()
            }
            AdminCommand::VerifyJson => {
                if body.len() > 2 && body[0].trim() == "```" && body.last().unwrap().trim() == "```"
                {
                    let string = body[1..body.len() - 1].join("\n");
                    match serde_json::from_str(&string) {
                        Ok(value) => {
                            let pub_key_map = RwLock::new(BTreeMap::new());

                            services()
                                .rooms
                                .event_handler
                                // Generally we shouldn't be checking against expired keys unless required, so in the admin
                                // room it might be best to not allow expired keys
                                .fetch_required_signing_keys(&value, &pub_key_map)
                                .await?;

                            let mut expired_key_map = BTreeMap::new();
                            let mut valid_key_map = BTreeMap::new();

                            for (server, keys) in pub_key_map.into_inner().into_iter() {
                                if keys.valid_until_ts > MilliSecondsSinceUnixEpoch::now() {
                                    valid_key_map.insert(
                                        server,
                                        keys.verify_keys
                                            .into_iter()
                                            .map(|(id, key)| (id, key.key))
                                            .collect(),
                                    );
                                } else {
                                    expired_key_map.insert(
                                        server,
                                        keys.verify_keys
                                            .into_iter()
                                            .map(|(id, key)| (id, key.key))
                                            .collect(),
                                    );
                                }
                            }

                            if ruma::signatures::verify_json(&valid_key_map, &value).is_ok() {
                                RoomMessageEventContent::text_plain("Signature correct")
                            } else if let Err(e) =
                                ruma::signatures::verify_json(&expired_key_map, &value)
                            {
                                RoomMessageEventContent::text_plain(format!(
                                    "Signature verification failed: {e}"
                                ))
                            } else {
                                RoomMessageEventContent::text_plain(
                                    "Signature correct (with expired keys)",
                                )
                            }
                        }
                        Err(e) => RoomMessageEventContent::text_plain(format!("Invalid json: {e}")),
                    }
                } else {
                    RoomMessageEventContent::text_plain(
                        "Expected code block in command body. Add --help for details.",
                    )
                }
                .into()
            }
            AdminCommand::HashAndSignEvent { room_version_id } => {
                if body.len() > 2
                    // Language may be specified as part of the codeblock (e.g. "```json")
                    && body[0].trim().starts_with("```")
                    && body.last().unwrap().trim() == "```"
                {
                    let string = body[1..body.len() - 1].join("\n");
                    match serde_json::from_str(&string) {
                        Ok(mut value) => {
                            if let Err(e) = ruma::signatures::hash_and_sign_event(
                                services().globals.server_name().as_str(),
                                services().globals.keypair(),
                                &mut value,
                                &room_version_id
                                    .rules()
                                    .expect("Supported room version has rules")
                                    .redaction,
                            ) {
                                RoomMessageEventContent::text_plain(format!("Invalid event: {e}"))
                            } else {
                                let json_text = serde_json::to_string_pretty(&value)
                                    .expect("canonical json is valid json");
                                RoomMessageEventContent::text_plain(json_text)
                            }
                        }
                        Err(e) => RoomMessageEventContent::text_plain(format!("Invalid json: {e}")),
                    }
                } else {
                    RoomMessageEventContent::text_plain(
                        "Expected code block in command body. Add --help for details.",
                    )
                }
                .into()
            }
            AdminCommand::RemoveAlias { alias } => {
                if alias.server_name() != services().globals.server_name() {
                    RoomMessageEventContent::text_plain(
                        "Cannot remove alias which is not from this server",
                    )
                } else if services()
                    .rooms
                    .alias
                    .resolve_local_alias(&alias)?
                    .is_none()
                {
                    RoomMessageEventContent::text_plain("No such alias exists")
                } else {
                    // We execute this as the server user for two reasons
                    // 1. If the user can execute commands in the admin room, they can always remove the alias.
                    // 2. In the future, we are likely going to be able to allow users to execute commands via
                    //   other methods, such as IPC, which would lead to us not knowing their user id

                    services()
                        .rooms
                        .alias
                        .remove_alias(&alias, services().globals.server_user())?;
                    RoomMessageEventContent::text_plain("Alias removed successfully")
                }
                .into()
            }
        };

        Ok(reply_message_content)
    }

    // Utility to turn clap's `--help` text to HTML.
    fn usage_to_html(&self, text: &str, server_name: &ServerName) -> String {
        // Replace `@conduit:servername:-subcmdname` with `@conduit:servername: subcmdname`
        let text = text.replace(
            &format!("@conduit:{server_name}:-"),
            &format!("@conduit:{server_name}: "),
        );

        // For the conduit admin room, subcommands become main commands
        let text = text.replace("SUBCOMMAND", "COMMAND");
        let text = text.replace("subcommand", "command");

        // Escape option names (e.g. `<element-id>`) since they look like HTML tags
        let text = text.replace('<', "&lt;").replace('>', "&gt;");

        // Italicize the first line (command name and version text)
        let re = Regex::new("^(.*?)\n").expect("Regex compilation should not fail");
        let text = re.replace_all(&text, "<em>$1</em>\n");

        // Unmerge wrapped lines
        let text = text.replace("\n            ", "  ");

        // Wrap option names in backticks. The lines look like:
        //     -V, --version  Prints version information
        // And are converted to:
        // <code>-V, --version</code>: Prints version information
        // (?m) enables multi-line mode for ^ and $
        let re = Regex::new("(?m)^    (([a-zA-Z_&;-]+(, )?)+)  +(.*)$")
            .expect("Regex compilation should not fail");
        let text = re.replace_all(&text, "<code>$1</code>: $4");

        // Look for a `[commandbody]()` tag. If it exists, use all lines below it that
        // start with a `#` in the USAGE section.
        let mut text_lines: Vec<&str> = text.lines().collect();
        let mut command_body = String::new();

        if let Some(line_index) = text_lines
            .iter()
            .position(|line| *line == "[commandbody]()")
        {
            text_lines.remove(line_index);

            while text_lines
                .get(line_index)
                .map(|line| line.starts_with('#'))
                .unwrap_or(false)
            {
                command_body += if text_lines[line_index].starts_with("# ") {
                    &text_lines[line_index][2..]
                } else {
                    &text_lines[line_index][1..]
                };
                command_body += "[nobr]\n";
                text_lines.remove(line_index);
            }
        }

        let text = text_lines.join("\n");

        // Improve the usage section
        let text = if command_body.is_empty() {
            // Wrap the usage line in code tags
            let re = Regex::new("(?m)^USAGE:\n    (@conduit:.*)$")
                .expect("Regex compilation should not fail");
            re.replace_all(&text, "USAGE:\n<code>$1</code>").to_string()
        } else {
            // Wrap the usage line in a code block, and add a yaml block example
            // This makes the usage of e.g. `register-appservice` more accurate
            let re = Regex::new("(?m)^USAGE:\n    (.*?)\n\n")
                .expect("Regex compilation should not fail");
            re.replace_all(&text, "USAGE:\n<pre>$1[nobr]\n[commandbodyblock]</pre>")
                .replace("[commandbodyblock]", &command_body)
        };

        // Add HTML line-breaks

        text.replace("\n\n\n", "\n\n")
            .replace('\n', "<br>\n")
            .replace("[nobr]<br>", "")
    }

    /// Create the admin room.
    ///
    /// Users in this room are considered admins by conduit, and the room can be
    /// used to issue admin commands by talking to the server user inside it.
    pub(crate) async fn create_admin_room(&self) -> Result<()> {
        // Create a user for the server
        let conduit_user = services().globals.server_user();

        services().users.create(conduit_user, None)?;

        let room_version = services().globals.default_room_version();
        let rules = room_version
            .rules()
            .expect("Supported room version must have rules.")
            .authorization;
        let mut content = if rules.use_room_create_sender {
            RoomCreateEventContent::new_v11()
        } else {
            RoomCreateEventContent::new_v1(conduit_user.to_owned())
        };
        content.federate = true;
        content.predecessor = None;
        content.room_version = room_version;

        // 1. The room create event
        let (room_id, mutex_state) = services()
            .rooms
            .timeline
            .send_create_room(
                to_raw_value(&content).expect("event is valid, we just created it"),
                conduit_user,
                &rules,
            )
            .await?;
        let state_lock = mutex_state.lock().await;

        // 2. Make conduit bot join
        services()
            .rooms
            .timeline
            .build_and_append_pdu(
                PduBuilder {
                    event_type: TimelineEventType::RoomMember,
                    content: to_raw_value(&RoomMemberEventContent {
                        membership: MembershipState::Join,
                        displayname: None,
                        avatar_url: None,
                        is_direct: None,
                        third_party_invite: None,
                        blurhash: None,
                        reason: None,
                        join_authorized_via_users_server: None,
                    })
                    .expect("event is valid, we just created it"),
                    unsigned: None,
                    state_key: Some(conduit_user.to_string()),
                    redacts: None,
                    timestamp: None,
                },
                conduit_user,
                &room_id,
                &state_lock,
            )
            .await?;

        // 3. Power levels
        let mut users = BTreeMap::new();
        if !rules.explicitly_privilege_room_creators {
            users.insert(conduit_user.to_owned(), 100.into());
        }

        services()
            .rooms
            .timeline
            .build_and_append_pdu(
                PduBuilder {
                    event_type: TimelineEventType::RoomPowerLevels,
                    content: to_raw_value(&RoomPowerLevelsEventContent {
                        users,
                        ..RoomPowerLevelsEventContent::new(&rules)
                    })
                    .expect("event is valid, we just created it"),
                    unsigned: None,
                    state_key: Some("".to_owned()),
                    redacts: None,
                    timestamp: None,
                },
                conduit_user,
                &room_id,
                &state_lock,
            )
            .await?;

        // 4.1 Join Rules
        services()
            .rooms
            .timeline
            .build_and_append_pdu(
                PduBuilder {
                    event_type: TimelineEventType::RoomJoinRules,
                    content: to_raw_value(&RoomJoinRulesEventContent::new(JoinRule::Invite))
                        .expect("event is valid, we just created it"),
                    unsigned: None,
                    state_key: Some("".to_owned()),
                    redacts: None,
                    timestamp: None,
                },
                conduit_user,
                &room_id,
                &state_lock,
            )
            .await?;

        // 4.2 History Visibility
        services()
            .rooms
            .timeline
            .build_and_append_pdu(
                PduBuilder {
                    event_type: TimelineEventType::RoomHistoryVisibility,
                    content: to_raw_value(&RoomHistoryVisibilityEventContent::new(
                        HistoryVisibility::Shared,
                    ))
                    .expect("event is valid, we just created it"),
                    unsigned: None,
                    state_key: Some("".to_owned()),
                    redacts: None,
                    timestamp: None,
                },
                conduit_user,
                &room_id,
                &state_lock,
            )
            .await?;

        // 4.3 Guest Access
        services()
            .rooms
            .timeline
            .build_and_append_pdu(
                PduBuilder {
                    event_type: TimelineEventType::RoomGuestAccess,
                    content: to_raw_value(&RoomGuestAccessEventContent::new(
                        GuestAccess::Forbidden,
                    ))
                    .expect("event is valid, we just created it"),
                    unsigned: None,
                    state_key: Some("".to_owned()),
                    redacts: None,
                    timestamp: None,
                },
                conduit_user,
                &room_id,
                &state_lock,
            )
            .await?;

        // 5. Events implied by name and topic
        let room_name = format!("{} Admin Room", services().globals.server_name());
        services()
            .rooms
            .timeline
            .build_and_append_pdu(
                PduBuilder {
                    event_type: TimelineEventType::RoomName,
                    content: to_raw_value(&RoomNameEventContent::new(room_name))
                        .expect("event is valid, we just created it"),
                    unsigned: None,
                    state_key: Some("".to_owned()),
                    redacts: None,
                    timestamp: None,
                },
                conduit_user,
                &room_id,
                &state_lock,
            )
            .await?;

        services()
            .rooms
            .timeline
            .build_and_append_pdu(
                PduBuilder {
                    event_type: TimelineEventType::RoomTopic,
                    content: to_raw_value(&RoomTopicEventContent::new(format!(
                        "Manage {}",
                        services().globals.server_name()
                    )))
                    .expect("event is valid, we just created it"),
                    unsigned: None,
                    state_key: Some("".to_owned()),
                    redacts: None,
                    timestamp: None,
                },
                conduit_user,
                &room_id,
                &state_lock,
            )
            .await?;

        // 6. Room alias
        let alias: OwnedRoomAliasId = services().globals.admin_alias().to_owned();

        services()
            .rooms
            .timeline
            .build_and_append_pdu(
                PduBuilder {
                    event_type: TimelineEventType::RoomCanonicalAlias,
                    content: to_raw_value(&RoomCanonicalAliasEventContent {
                        alias: Some(alias.clone()),
                        alt_aliases: Vec::new(),
                    })
                    .expect("event is valid, we just created it"),
                    unsigned: None,
                    state_key: Some("".to_owned()),
                    redacts: None,
                    timestamp: None,
                },
                conduit_user,
                &room_id,
                &state_lock,
            )
            .await?;

        services()
            .rooms
            .alias
            .set_alias(&alias, &room_id, conduit_user)?;

        Ok(())
    }

    /// Gets the room ID of the admin room
    ///
    /// Errors are propagated from the database, and will have None if there is no admin room
    pub(crate) fn get_admin_room(&self) -> Result<Option<OwnedRoomId>> {
        services()
            .rooms
            .alias
            .resolve_local_alias(services().globals.admin_alias())
    }

    /// Invite the user to the conduit admin room.
    ///
    /// In conduit, this is equivalent to granting admin privileges.
    pub(crate) async fn make_user_admin(
        &self,
        user_id: &UserId,
        displayname: String,
    ) -> Result<()> {
        if let Some(room_id) = services().admin.get_admin_room()? {
            let mutex_state = Arc::clone(
                services()
                    .globals
                    .roomid_mutex_state
                    .write()
                    .await
                    .entry(room_id.clone())
                    .or_default(),
            );
            let state_lock = mutex_state.lock().await;

            // Use the server user to grant the new admin's power level
            let conduit_user = services().globals.server_user();

            // Invite and join the real user
            services()
                .rooms
                .timeline
                .build_and_append_pdu(
                    PduBuilder {
                        event_type: TimelineEventType::RoomMember,
                        content: to_raw_value(&RoomMemberEventContent {
                            membership: MembershipState::Invite,
                            displayname: None,
                            avatar_url: None,
                            is_direct: None,
                            third_party_invite: None,
                            blurhash: None,
                            reason: None,
                            join_authorized_via_users_server: None,
                        })
                        .expect("event is valid, we just created it"),
                        unsigned: None,
                        state_key: Some(user_id.to_string()),
                        redacts: None,
                        timestamp: None,
                    },
                    conduit_user,
                    &room_id,
                    &state_lock,
                )
                .await?;
            services()
                .rooms
                .timeline
                .build_and_append_pdu(
                    PduBuilder {
                        event_type: TimelineEventType::RoomMember,
                        content: to_raw_value(&RoomMemberEventContent {
                            membership: MembershipState::Join,
                            displayname: Some(displayname),
                            avatar_url: None,
                            is_direct: None,
                            third_party_invite: None,
                            blurhash: None,
                            reason: None,
                            join_authorized_via_users_server: None,
                        })
                        .expect("event is valid, we just created it"),
                        unsigned: None,
                        state_key: Some(user_id.to_string()),
                        redacts: None,
                        timestamp: None,
                    },
                    user_id,
                    &room_id,
                    &state_lock,
                )
                .await?;

            // Set power level
            let room_version = services().rooms.state.get_room_version(&room_id)?;
            let rules = room_version
                .rules()
                .expect("Supported room version must have rules.")
                .authorization;

            let mut users = BTreeMap::new();
            if !rules.explicitly_privilege_room_creators {
                users.insert(conduit_user.to_owned(), 100.into());
            }
            users.insert(user_id.to_owned(), 100.into());

            services()
                .rooms
                .timeline
                .build_and_append_pdu(
                    PduBuilder {
                        event_type: TimelineEventType::RoomPowerLevels,
                        content: to_raw_value(&RoomPowerLevelsEventContent {
                            users,
                            ..RoomPowerLevelsEventContent::new(&rules)
                        })
                        .expect("event is valid, we just created it"),
                        unsigned: None,
                        state_key: Some("".to_owned()),
                        redacts: None,
                        timestamp: None,
                    },
                    conduit_user,
                    &room_id,
                    &state_lock,
                )
                .await?;

            // Send welcome message
            services().rooms.timeline.build_and_append_pdu(
            PduBuilder {
                event_type: TimelineEventType::RoomMessage,
                content: to_raw_value(&RoomMessageEventContent::text_html(
                        format!("## Thank you for trying out Conduit!\n\nConduit is currently in Beta. This means you can join and participate in most Matrix rooms, but not all features are supported and you might run into bugs from time to time.\n\nHelpful links:\n> Website: https://conduit.rs\n> Git and Documentation: https://gitlab.com/famedly/conduit\n> Report issues: https://gitlab.com/famedly/conduit/-/issues\n\nFor a list of available commands, send the following message in this room: `@conduit:{}: --help`\n\nHere are some rooms you can join (by typing the command):\n\nConduit room (Ask questions and get notified on updates):\n`/join #conduit:ahimsa.chat`\n\nConduit lounge (Off-topic, only Conduit users are allowed to join)\n`/join #conduit-lounge:conduit.rs`", services().globals.server_name()),
                        format!("<h2>Thank you for trying out Conduit!</h2>\n<p>Conduit is currently in Beta. This means you can join and participate in most Matrix rooms, but not all features are supported and you might run into bugs from time to time.</p>\n<p>Helpful links:</p>\n<blockquote>\n<p>Website: https://conduit.rs<br>Git and Documentation: https://gitlab.com/famedly/conduit<br>Report issues: https://gitlab.com/famedly/conduit/-/issues</p>\n</blockquote>\n<p>For a list of available commands, send the following message in this room: <code>@conduit:{}: --help</code></p>\n<p>Here are some rooms you can join (by typing the command):</p>\n<p>Conduit room (Ask questions and get notified on updates):<br><code>/join #conduit:ahimsa.chat</code></p>\n<p>Conduit lounge (Off-topic, only Conduit users are allowed to join)<br><code>/join #conduit-lounge:conduit.rs</code></p>\n", services().globals.server_name()),
                ))
                .expect("event is valid, we just created it"),
                unsigned: None,
                state_key: None,
                redacts: None,
                timestamp: None,
            },
            conduit_user,
            &room_id,
            &state_lock,
        ).await?;
        }
        Ok(())
    }

    /// Checks whether a given user is an admin of this server
    pub fn user_is_admin(&self, user_id: &UserId) -> Result<bool> {
        let Some(admin_room) = self.get_admin_room()? else {
            return Ok(false);
        };

        services().rooms.state_cache.is_joined(user_id, &admin_room)
    }
}

fn userids_from_body<'a>(
    body: &'a [&'a str],
) -> Result<Result<Vec<&'a UserId>, MessageType>, Error> {
    let users = body.to_owned().drain(1..body.len() - 1).collect::<Vec<_>>();

    let mut user_ids = Vec::new();
    let mut remote_ids = Vec::new();
    let mut non_existent_ids = Vec::new();
    let mut invalid_users = Vec::new();

    for &user in &users {
        match <&UserId>::try_from(user) {
            Ok(user_id) => {
                if user_id.server_name() != services().globals.server_name() {
                    remote_ids.push(user_id)
                } else if !services().users.exists(user_id)? {
                    non_existent_ids.push(user_id)
                } else {
                    user_ids.push(user_id)
                }
            }
            Err(_) => {
                invalid_users.push(user);
            }
        }
    }

    let mut markdown_message = String::new();
    let mut html_message = String::new();
    if !invalid_users.is_empty() {
        markdown_message.push_str("The following user ids are not valid:\n```\n");
        html_message.push_str("The following user ids are not valid:\n<pre>\n");
        for invalid_user in invalid_users {
            markdown_message.push_str(&format!("{invalid_user}\n"));
            html_message.push_str(&format!("{invalid_user}\n"));
        }
        markdown_message.push_str("```\n\n");
        html_message.push_str("</pre>\n\n");
    }
    if !remote_ids.is_empty() {
        markdown_message.push_str("The following users are not from this server:\n```\n");
        html_message.push_str("The following users are not from this server:\n<pre>\n");
        for remote_id in remote_ids {
            markdown_message.push_str(&format!("{remote_id}\n"));
            html_message.push_str(&format!("{remote_id}\n"));
        }
        markdown_message.push_str("```\n\n");
        html_message.push_str("</pre>\n\n");
    }
    if !non_existent_ids.is_empty() {
        markdown_message.push_str("The following users do not exist:\n```\n");
        html_message.push_str("The following users do not exist:\n<pre>\n");
        for non_existent_id in non_existent_ids {
            markdown_message.push_str(&format!("{non_existent_id}\n"));
            html_message.push_str(&format!("{non_existent_id}\n"));
        }
        markdown_message.push_str("```\n\n");
        html_message.push_str("</pre>\n\n");
    }
    if !markdown_message.is_empty() {
        return Ok(Err(RoomMessageEventContent::text_html(
            markdown_message,
            html_message,
        )
        .into()));
    }

    Ok(Ok(user_ids))
}

fn media_from_body(body: Vec<&str>) -> Result<Vec<(OwnedServerName, String)>, MessageType> {
    if body.len() > 2 && body[0].trim() == "```" && body.last().unwrap().trim() == "```" {
        Ok(body
            .clone()
            .drain(1..body.len() - 1)
            .map(<Box<MxcUri>>::from)
            .filter_map(|mxc| {
                mxc.parts()
                    .map(|(server_name, media_id)| (server_name.to_owned(), media_id.to_owned()))
                    .ok()
            })
            .collect::<Vec<_>>())
    } else {
        Err(RoomMessageEventContent::text_plain(
            "Expected code block in command body. Add --help for details.",
        )
        .into())
    }
}

fn unix_secs_from_duration(duration: Duration) -> Result<u64> {
    SystemTime::now()
        .checked_sub(duration).ok_or_else(||Error::AdminCommand("Given timeframe cannot be represented as system time, please try again with a shorter time-frame"))
        .map(|time| time
                .duration_since(UNIX_EPOCH)
                .expect("Time is after unix epoch")
                .as_secs())
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn get_help_short() {
        get_help_inner("-h");
    }

    #[test]
    fn get_help_long() {
        get_help_inner("--help");
    }

    #[test]
    fn get_help_subcommand() {
        get_help_inner("help");
    }

    fn get_help_inner(input: &str) {
        let error = AdminCommand::try_parse_from(["argv[0] doesn't matter", input])
            .unwrap_err()
            .to_string();

        // Search for a handful of keywords that suggest the help printed properly
        assert!(error.contains("Usage:"));
        assert!(error.contains("Commands:"));
        assert!(error.contains("Options:"));
    }
}
