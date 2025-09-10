# Element Call Implementation Plan

This document outlines the necessary steps to implement support for Element Call in Conduit.

## 1. Architecture Overview

Element Call uses a multi-component architecture. The Matrix homeserver (Conduit) is responsible for two key areas:

1.  **Signaling:** Reliably routing the specific Matrix events (defined in MSC3401) that clients use to set up and manage calls.
2.  **TURN Server Configuration:** Providing clients with the credentials for a TURN/STUN server, which is necessary for WebRTC connections to traverse NATs and firewalls.

Group calls are managed by a "focus" user, which is an Application Service that the homeserver must be able to communicate with.

## 2. Homeserver Implementation Tasks

To support Element Call, Conduit needs to implement the following:

### Task A: TURN Server Endpoint

- **Requirement:** Implement the `GET /_matrix/client/v1/voip/turnServer` endpoint.
- **Functionality:** This endpoint should return the URLs and credentials for a configured TURN/STUN server.
- **Configuration:** A new section must be added to `conduit.toml` to allow administrators to configure their TURN server details (URI, username, and a shared secret for generating credentials).

### Task B: MSC3401 Native Group VoIP Signaling

- **Requirement:** Ensure Conduit can correctly process and route the custom to-device and room events defined in MSC3401.
- **Analysis:** This requires verifying that the version of the `ruma` crate used by Conduit includes support for these new event types. If not, `ruma` will need to be updated.
- **Functionality:** No special logic is required beyond the standard event handling and routing, as the logic resides on the clients and the focus.

### Task C: Support for a Focus Application Service

- **Requirement:** Ensure Conduit's Application Service framework is capable of supporting a focus user.
- **Functionality:** The homeserver must be able to:
    - Register the focus as an exclusive user.
    - Route all events for the focus user to the Application Service.
    - Allow the Application Service to send events as the focus user.
- **Analysis:** This will likely require reviewing and potentially enhancing the existing appservice implementation in `src/service/appservice/`.

## 3. User-Facing Documentation

- **Requirement:** Add a new section to the Conduit documentation explaining how to set up and configure Element Call.
- **Content:** This should include:
    - How to configure the TURN server in `conduit.toml`.
    - Instructions on how to set up and register a focus Application Service.
    - An example `docker-compose.yml` file showing how to run Conduit alongside a focus and a TURN server.
