# Conduit Federation Sender Architecture

## High-Level Overview

The federation sender is an asynchronous, queue-based system responsible for reliably sending events (PDUs like messages, and EDUs like typing notifications) to other Matrix homeservers.

Its core design is to decouple the generation of an event from the process of sending it. When a part of Conduit needs to send an event to another server, it simply adds it to a persistent queue. A dedicated background worker then processes this queue, bundling events into transactions, handling network requests, and managing retries for failed attempts.

The key architectural goals are:
1.  **Reliability:** Ensure events are eventually sent, even if the remote server is temporarily offline.
2.  **Efficiency:** Bundle multiple events into a single transaction to reduce network overhead.
3.  **Consistency:** Ensure that real-time status information (like typing and read receipts) is kept synchronized.

---

## Core Components & Data Flow

Here is a breakdown of the main components and how an event flows through the system.

```
+---------------------------------+      +---------------------------------+
| Event Source                    |      | Typing Timeout Task             |
| (e.g., new message, typing...)  |      | (background job, every 1 sec)   |
+---------------------------------+      +---------------------------------+
             |                                           |
             | 1. Event is generated                     | 1b. Timeout is detected
             |                                           |
             v                                           v
+--------------------------------------------------------------------------+
| `sending::Service`                                                       |
|   - `sender`: An MPSC channel (in-memory queue for new events)           |
|   - `db`: The persistent database queue                                  |
+--------------------------------------------------------------------------+
             | 2. Event is sent to the channel
             v
+--------------------------------------------------------------------------+
| `handler()` Background Worker (The Core Loop)                            |
|                                                                          |
|   - `select! { ... }`: Listens for new events or timer ticks.            |
|   - `current_transaction_status`: Tracks state of each remote server.    |
|                                                                          |
|   3. Receives new event from channel.                                    |
|   4. Calls `select_events()` to decide if a transaction should start.    |
|      - If a transaction is already running for that server, the new      |
|        event is added to an in-memory queue (`Running(Vec<...>)`).       |
|                                                                          |
|   5. If a new transaction starts, it calls `handle_events()`.            |
|                                                                          |
+--------------------------------------------------------------------------+
             |
             v
+--------------------------------------------------------------------------+
| `handle_events()` (Transaction Builder)                                  |
|                                                                          |
|   6. Gathers all PDUs for the transaction from the queue.                |
|   7. **Crucially**, it calls `select_edus()` to get a fresh snapshot     |
|      of the *current* typing and read-receipt state.                     |
|   8. Bundles PDUs and fresh EDUs into a single transaction.              |
|                                                                          |
+--------------------------------------------------------------------------+
             | 9. Sends the transaction
             v
+--------------------------------------------------------------------------+
| `server_server::send_request()`                                          |
|   - Handles HTTP signing, DNS resolution, and the actual network request.|
+--------------------------------------------------------------------------+
             | 10. Sends to remote server
             v
      [ Remote Homeserver ]
```

---

## Key Architectural Decisions & Recent Fixes

The recent debugging has led to several critical architectural changes that address the reliability issues you observed.

### 1. State-Based vs. Event-Based EDUs (The "Works Once Per Room" Fix)

*   **Problem:** The old system treated every "start typing" and "stop typing" signal as a unique event to be queued. If a single event was dropped or delayed, the state on the remote server would become permanently desynchronized.
*   **Solution:** The architecture was changed to be **state-based**. The `select_edus` function no longer looks at a queue of past events. Instead, it actively queries the current state of the system: "Who is typing in this room *right now*?".
*   **Impact:** Every transaction sent to a remote server now contains a fresh, accurate snapshot of the current typing and read-receipt state. This is self-correcting; even if a previous update was missed, the next transaction will automatically fix the state on the remote server. This is the standard, robust architecture used by mature homeservers.

### 2. Proactive Queue Worker (The "Needs a Message to Reset" Fix)

*   **Problem:** The background `handler` worker would only wake up when a new event was added to its in-memory channel. If events were already waiting in the persistent *database* queue (e.g., from a failed transaction), the worker could go to sleep and never wake up to retry them. Receiving a new message would "kick" the worker by adding a read receipt to the queue, forcing it to process the backlog.
*   **Solution:** I added a periodic timer (`tokio::time::interval`) to the worker's main `select!` loop. Now, every 60 seconds, the worker is forced to wake up and check for any failed transactions that are ready to be retried, regardless of whether new events have come in.
*   **Impact:** The federation queue can no longer get "stuck". Events will always be retried and sent in a timely manner, without relying on external triggers.

### 3. Dedicated Typing Timeout Task (The "Stop-Typing on Timeout" Fix)

*   **Problem:** A user who stops typing (by going idle) should have their typing notification cleared. The logic for this existed but was only triggered passively when a client made a `/sync` request. It was not proactive.
*   **Solution:** I created a new, dedicated background task (`typing::typings_maintain_task`) that runs on its own timer (now set to every 1 second). Its only job is to find all users whose typing status has timed out and generate a "stop typing" (`typing: false`) event for them.
*   **Impact:** Typing notifications are now reliably cleared within a second of their timeout, making the state more synchronized and responsive.
