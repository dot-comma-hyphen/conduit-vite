# TODO: Refactor `/sync` Endpoint for True Long-Polling (Revised)

This plan addresses the root cause of the perceived desynchronization of real-time events like typing notifications. The issue lies in a fundamental architectural flaw in the `/sync` endpoint's long-polling logic.

## The Bug: A Flawed "Build-Then-Wait" Pattern

The current implementation of the `sync_helper` function in `src/api/client_server/sync.rs` follows an incorrect "build-then-wait" pattern:
1.  It builds the *entire* sync response based on the state of the server at that moment.
2.  It then checks if the generated response is empty.
3.  Only if the response is completely empty does it wait for a notification (like a new message or a typing update).

This means that if the *only* new event is a low-priority one like `m.typing`, the response is considered non-empty, and the server returns it immediately. This prevents true long-polling, causing the client to immediately reconnect in a rapid "spin-poll" loop, which feels laggy and inefficient.

## The Solution: Refactor to a "Wait-Then-Build" Pattern

The `sync_helper` function must be refactored to follow the standard and correct "wait-then-build" long-polling pattern. This will make it more efficient and ensure all real-time events feel instantaneous.

### Implementation Plan

The core of the work is to restructure the `sync_helper` function.

*   **File:** `src/api/client_server/sync.rs`
*   **Function:** `sync_helper`
*   **Architectural Change:**

    1.  **Introduce a Main Loop:** The function should be structured around a `loop`.

    2.  **Check for Changes First:** At the beginning of each loop iteration, the function will perform a series of fast, efficient checks to see if any data has changed since the client's `since` token. This includes checking for:
        *   New timeline events (messages).
        *   New state events.
        *   New account data.
        *   New to-device messages.
        *   Changes in presence.
        *   **Crucially, changes in typing status** by comparing `last_typing_update` with `since`.

    3.  **Immediate Return on Change:** If *any* of the above checks find new data, the function will proceed to build the full, detailed `sync_events::v3::Response` and **return it immediately**.

    4.  **Wait for Notifications:** If the initial checks find **no changes**, the function will then enter the `tokio::select!` block. It will wait on all notification watchers simultaneously:
        *   The main `watcher` for new PDUs.
        *   The `typing_receiver` for new typing updates.
        *   The client-specified `timeout`.

    5.  **Handle Watcher Results:**
        *   If the `watcher` or `typing_receiver` fires, it means a new event has arrived. The `select!` block will exit, and the main `loop` will **continue** to its next iteration, which will now detect the change in step #2 and return a response.
        *   If the `timeout` is reached, the `select!` block will exit, and the function will return a completely empty response with an updated `next_batch` token, completing the long-poll cycle.

This architectural change will fix the typing notification issue definitively and make the entire `/sync` endpoint more robust, efficient, and aligned with the Matrix specification and reference implementations.
