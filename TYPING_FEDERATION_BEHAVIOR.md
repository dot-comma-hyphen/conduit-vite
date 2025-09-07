### The `m.typing` Federation Model: A Bus Schedule vs. a Taxi

The core concept to understand is that Conduit's `m.typing` federation has been intentionally designed as a **state-based system that operates like a bus service**, not an event-based system that operates like a taxi service. This is for efficiency and reliability.

*   **The Old (Buggy) System - A Taxi Service:** The moment a user started or stopped typing, a dedicated "taxi" (an individual EDU) was dispatched to the federation queue. This was instant but led to race conditions where the "stop typing" taxi could arrive before the "start typing" taxi, causing a user's typing indicator to get stuck on.

*   **The New (Correct) System - A Bus Service:** The "bus" is the federation transaction that your server sends to another server. It's much more efficient to send one bus with several passengers (events) than a separate taxi for every single person.

A bus leaves the station under one of two conditions:
1.  **It's time for its scheduled departure (The Periodic Timer).** The federation worker runs on a timer (currently every 3 seconds) and will create and send a transaction to any servers it needs to communicate with.
2.  **A passenger with an urgent package arrives (An Outgoing Event).** If a more urgent event, like a message (PDU) or a read receipt (EDU), needs to be sent, it triggers the bus to depart immediately, without waiting for the schedule.

---

### How the "Hitching a Ride" Scenario Unfolds

This model explains the specific behavior you observed: you are typing, but your friend doesn't see it until *they* perform an action.

**Step 1: You Start Typing**

*   You start typing a message.
*   **On your Conduit server:** The server's internal state is immediately updated: `{ "your_user": "is_typing" }`.
*   **Federation:** Nothing is sent yet. There are no urgent packages. The bus is waiting at the station for its next scheduled departure.

**Step 2: Your Friend Starts Typing (or Sends a Message)**

*   Your friend on another server starts typing or sends a message.
*   **Federation:** Their server sends a transaction to your Conduit server.

**Step 3: The "Hitching a Ride" Mechanism**

*   **A New "Urgent Package" is Created:** Your client application (e.g., Element) sees the incoming event from your friend. To acknowledge this, it automatically sends a **read receipt** (`m.receipt`) back to your Conduit server. This read receipt is now an "urgent package" that needs to be sent back to your friend's server.

*   **The Bus Departs Immediately:** The federation `handler` sees this new, queued read receipt. It doesn't wait for the timer. It immediately spawns a worker to create and send a transaction to your friend's server *right now*.

*   **The State is Captured:** Before the bus leaves, the worker runs the `select_edus` function. This function does two things:
    1.  It grabs the urgent package (the `m.receipt` EDU).
    2.  It checks the server's **current state**. It looks at the typing map and sees `{ "your_user": "is_typing" }`. It generates a fresh `m.typing` EDU with `typing: true` as part of its state report.

*   **The Transaction is Sent:** The transaction is sent to your friend's server containing **both** the read receipt and your `m.typing` status.

**The Result:** From your friend's perspective, the moment they acted, they almost instantly saw your typing notification appear. It wasn't their typing that *directly* triggered yours; it was the **read receipt** their action generated from your client that provided a vehicle for your typing status to "hitch a ride" on. This is the intended, efficient behavior of the state-based system.
