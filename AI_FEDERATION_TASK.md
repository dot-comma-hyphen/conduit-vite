# Federation Implementation Plan for m.typing

## Research Summary

The goal is to implement federation for `m.typing` notifications in Conduit.

1.  **Synapse (Reference Homeserver):**
    *   Handles typing notifications from clients via a `TypingHandler`.
    *   This handler queues the event for a `FederationSender`.
    *   The `FederationSender` constructs an `m.typing` EDU and sends it to all remote servers participating in the room.

2.  **Dendrite (Go Homeserver):**
    *   Follows a similar, modular pattern.
    *   A room server component processes the initial typing event.
    *   The event is placed into a dedicated queue for a `federation-sender` microservice.
    *   The sender service packages the `m.typing` EDU into a transaction and sends it to remote servers.

3.  **Matrix Specification:**
    *   Confirms that `m.typing` events are sent over federation as Ephemeral Data Units (EDUs).
    *   The EDU `content` must include `room_id`, `user_id`, and a boolean `typing` field.

4.  **Current Conduit Implementation:**
    *   The existing `typing` service (`src/service/rooms/edus/typing/mod.rs`) only manages the local state of typing events within the server.
    *   It currently lacks any logic to send these events to other federated servers.

## Implementation Plan

The plan is to integrate the federation logic into the existing services, following the pattern established by Synapse and Dendrite.

1.  **Modify the `typing` service (`src/service/rooms/edus/typing/mod.rs`):**
    *   In the `typing_add` and `typing_remove` functions, add a call to a new function in the `sending` service.
    *   This call will pass the `user_id`, `room_id`, and the typing status (`true` for `typing_add`, `false` for `typing_remove`).

2.  **Create a new function in the `sending` service (`src/service/sending/mod.rs`):**
    *   Create a new public function: `send_federation_typing_edu`.
    *   This function will perform the following steps:
        *   Get a list of all servers in the given `room_id` using the `state_cache` service.
        *   Filter out our own server name from the list.
        *   If the resulting list of remote servers is not empty, construct an `m.typing` EDU using the provided `user_id`, `room_id`, and `typing` status.
        *   Call the existing `send_federation_edu` function to queue the EDU for delivery to all the remote servers.
