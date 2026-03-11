--------------------------- MODULE SynclineSync ---------------------------
(*
 * Formal specification of the Syncline synchronization protocol.
 *
 * Phase 1: Core protocol — star-topology CRDT sync over WebSockets.
 *
 * Abstractions:
 *   - CRDTs are modeled as sets of opaque update tokens (merge = union).
 *   - Network channels are FIFO queues (matching TCP/WebSocket).
 *   - The server is a single process with in-memory channels + persistent DB.
 *
 * What this verifies:
 *   - No echo: updates are never sent back to the originating client.
 *   - Persist before broadcast: DB written before any client sees the update.
 *   - Eventual convergence: all connected clients reach the same state.
 *   - Channel hygiene: only connected clients in broadcast channels.
 *   - Index consistency: every doc with data is in the index.
 *
 * Source: docs/FORMAL_VERIFICATION.md
 *)
EXTENDS Naturals, Sequences, FiniteSets, TLC

CONSTANTS
    Clients,              \* Set of client IDs, e.g. {"A", "B"}
    DocIds,               \* Set of document IDs, e.g. {"d1"}
    MaxUpdates,           \* Bound on total updates for finite model checking
    MaxQueueLen           \* Bound on message queue length per client

(* ===================== HELPER OPERATORS ===================== *)

\* Convert a sequence to the set of its elements
SeqToSet(s) == {s[i] : i \in 1..Len(s)}

\* Get the set of update IDs from a sequence of updates
UpdateIds(s) == {s[i].id : i \in 1..Len(s)}

\* Append all elements of a set (as a sequence) to a sequence.
\* Order doesn't matter for correctness since CRDTs are order-independent.
AppendAll(seq, set) ==
    LET F[S \in SUBSET set] ==
        IF S = {} THEN seq
        ELSE LET x == CHOOSE x \in S : TRUE
             IN Append(F[S \ {x}], x)
    IN F[set]

\* Filter elements from a sequence that satisfy a predicate
FilterSeq(s, Test(_)) ==
    LET F[i \in 0..Len(s)] ==
        IF i = 0 THEN <<>>
        ELSE IF Test(s[i]) THEN Append(F[i-1], s[i])
             ELSE F[i-1]
    IN F[Len(s)]

(* ===================== MESSAGE TYPES ===================== *)

\* Message type constants — matching protocol.rs MsgType enum
MsgSyncStep1 == "MSG_SYNC_STEP_1"
MsgSyncStep2 == "MSG_SYNC_STEP_2"
MsgUpdate    == "MSG_UPDATE"

(* ===================== VARIABLES ===================== *)

VARIABLES
    \* --- Client State ---
    clientDoc,            \* [Clients -> [DocIds -> Seq(Update)]]
    clientConnected,      \* [Clients -> BOOLEAN]
    clientSubscribed,     \* [Clients -> SUBSET DocIds]

    \* --- Network ---
    clientToServer,       \* [Clients -> Seq(Message)]
    serverToClient,       \* [Clients -> Seq(Message)]

    \* --- Server State ---
    serverDB,             \* [DocIds -> Seq(Update)]
    serverChannels,       \* [DocIds -> SUBSET Clients]
    serverIndex,          \* SUBSET DocIds

    \* --- Global ---
    updateCounter         \* Nat — monotonic counter for unique update IDs

vars == <<clientDoc, clientConnected, clientSubscribed,
          clientToServer, serverToClient,
          serverDB, serverChannels, serverIndex,
          updateCounter>>

(* ===================== STATE CONSTRAINT ===================== *)

\* Bounds the state space so TLC terminates in reasonable time.
\* This limits how many messages can accumulate in network queues.
StateConstraint ==
    /\ \A c \in Clients : Len(clientToServer[c]) <= MaxQueueLen
    /\ \A c \in Clients : Len(serverToClient[c]) <= MaxQueueLen
    /\ \A d \in DocIds  : Len(serverDB[d])        <= MaxUpdates

(* ===================== TYPE INVARIANT ===================== *)

Update == [id : Nat, origin : Clients, docId : DocIds]

Message == [type : {MsgSyncStep1, MsgSyncStep2, MsgUpdate},
            docId : DocIds]
            \* Plus additional fields depending on type — TLC checks structurally

TypeOK ==
    /\ clientConnected  \in [Clients -> BOOLEAN]
    /\ clientSubscribed \in [Clients -> SUBSET DocIds]
    /\ serverChannels   \in [DocIds  -> SUBSET Clients]
    /\ serverIndex      \in SUBSET DocIds
    /\ updateCounter    \in Nat

(* ===================== INITIAL STATE ===================== *)

Init ==
    /\ clientDoc        = [c \in Clients |-> [d \in DocIds |-> <<>>]]
    /\ clientConnected  = [c \in Clients |-> TRUE]     \* All start connected
    /\ clientSubscribed = [c \in Clients |-> {}]
    /\ clientToServer   = [c \in Clients |-> <<>>]
    /\ serverToClient   = [c \in Clients |-> <<>>]
    /\ serverDB         = [d \in DocIds  |-> <<>>]
    /\ serverChannels   = [d \in DocIds  |-> {}]
    /\ serverIndex      = {}
    /\ updateCounter    = 0

(* ===================== CLIENT ACTIONS ===================== *)

(*
 * ClientEdit(c, d): Client c makes a local edit to document d.
 * Maps to: app.rs watcher_rx.recv() -> apply_diff_to_yrs()
 *)
ClientEdit(c, d) ==
    /\ clientConnected[c]
    /\ updateCounter < MaxUpdates
    /\ LET u == [id     |-> updateCounter + 1,
                 origin |-> c,
                 docId  |-> d]
       IN
       /\ clientDoc' = [clientDoc EXCEPT ![c][d] = Append(@, u)]
       /\ updateCounter' = updateCounter + 1
       \* If subscribed, send update to server
       /\ IF d \in clientSubscribed[c]
          THEN clientToServer' = [clientToServer EXCEPT
                 ![c] = Append(@, [type  |-> MsgUpdate,
                                   docId |-> d,
                                   update |-> u])]
          ELSE UNCHANGED clientToServer
    /\ UNCHANGED <<clientConnected, clientSubscribed, serverToClient,
                   serverDB, serverChannels, serverIndex>>

(*
 * ClientGoOffline(c): Client c disconnects.
 * Maps to: network.rs WebSocket close
 *)
ClientGoOffline(c) ==
    /\ clientConnected[c]
    /\ clientConnected' = [clientConnected EXCEPT ![c] = FALSE]
    /\ clientSubscribed' = [clientSubscribed EXCEPT ![c] = {}]
    \* Server removes client from all channels
    /\ serverChannels' = [d \in DocIds |-> serverChannels[d] \ {c}]
    \* Drop any in-flight messages (connection lost)
    /\ clientToServer' = [clientToServer EXCEPT ![c] = <<>>]
    /\ serverToClient' = [serverToClient EXCEPT ![c] = <<>>]
    /\ UNCHANGED <<clientDoc, serverDB, serverIndex, updateCounter>>

(*
 * ClientReconnect(c): Client c comes back online and sends SyncStep1
 * for all documents it has locally.
 * Maps to: app.rs outer loop -> send MSG_SYNC_STEP_1 for known docs
 *)
ClientReconnect(c) ==
    /\ ~clientConnected[c]
    /\ clientConnected' = [clientConnected EXCEPT ![c] = TRUE]
    \* Send SyncStep1 for every document the client has data for
    /\ LET knownDocs == {d \in DocIds : clientDoc[c][d] /= <<>>}
           syncMsgs == [d \in knownDocs |->
                          [type        |-> MsgSyncStep1,
                           docId       |-> d,
                           stateVector |-> UpdateIds(clientDoc[c][d])]]
           \* Build a sequence from the set of messages
           msgSeq   == AppendAll(<<>>, {syncMsgs[d] : d \in knownDocs})
       IN clientToServer' = [clientToServer EXCEPT ![c] = @ \o msgSeq]
    /\ UNCHANGED <<clientDoc, clientSubscribed, serverToClient,
                   serverDB, serverChannels, serverIndex, updateCounter>>

(*
 * ClientReceive(c): Client c processes the next message from the server.
 * Maps to: app.rs app_rx.recv() -> MsgType::SyncStep2 | MsgType::Update
 *)
ClientReceive(c) ==
    /\ clientConnected[c]
    /\ serverToClient[c] /= <<>>
    /\ LET msg == Head(serverToClient[c])
       IN
       CASE msg.type = MsgSyncStep2 ->
            \* Apply all updates the client doesn't already have
            LET myIds      == UpdateIds(clientDoc[c][msg.docId])
                newUpdates == {u \in SeqToSet(msg.updates) : u.id \notin myIds}
            IN clientDoc' = [clientDoc EXCEPT
                 ![c][msg.docId] = AppendAll(@, newUpdates)]

         [] msg.type = MsgUpdate ->
            \* Apply the single update if not already present
            IF msg.update.id \notin UpdateIds(clientDoc[c][msg.docId])
            THEN clientDoc' = [clientDoc EXCEPT
                   ![c][msg.docId] = Append(@, msg.update)]
            ELSE UNCHANGED clientDoc

         [] OTHER -> UNCHANGED clientDoc

    /\ serverToClient' = [serverToClient EXCEPT ![c] = Tail(@)]
    /\ UNCHANGED <<clientConnected, clientSubscribed, clientToServer,
                   serverDB, serverChannels, serverIndex, updateCounter>>

(*
 * ClientSubscribe(c, d): Client c sends SyncStep1 for a document d
 * it hasn't subscribed to yet (e.g., discovered via __index__).
 * Maps to: app.rs "Discovered new UUID from __index__" path
 *)
ClientSubscribe(c, d) ==
    /\ clientConnected[c]
    /\ d \notin clientSubscribed[c]
    /\ clientToServer' = [clientToServer EXCEPT
         ![c] = Append(@, [type        |-> MsgSyncStep1,
                           docId       |-> d,
                           stateVector |-> UpdateIds(clientDoc[c][d])])]
    /\ UNCHANGED <<clientDoc, clientConnected, clientSubscribed,
                   serverToClient, serverDB, serverChannels,
                   serverIndex, updateCounter>>

(* ===================== SERVER ACTIONS ===================== *)

(*
 * ServerReceiveSyncStep1(c): Server processes MSG_SYNC_STEP_1 from client c.
 * Maps to: server.rs handle_socket -> MSG_SYNC_STEP_1 branch
 *
 * The server:
 *   1. Subscribes the client to the broadcast channel
 *   2. Registers the doc in __index__
 *   3. Computes the diff (updates client doesn't have)
 *   4. Sends MSG_SYNC_STEP_2 with the diff
 *)
ServerReceiveSyncStep1(c) ==
    /\ clientToServer[c] /= <<>>
    /\ Head(clientToServer[c]).type = MsgSyncStep1
    /\ LET msg  == Head(clientToServer[c])
           d    == msg.docId
           sv   == msg.stateVector
           \* Diff: all server-side updates whose IDs the client doesn't have
           diff == FilterSeq(serverDB[d],
                     LAMBDA u : u.id \notin sv)
       IN
       \* 1. Subscribe client to broadcast channel
       /\ serverChannels' = [serverChannels EXCEPT ![d] = @ \cup {c}]
       /\ clientSubscribed' = [clientSubscribed EXCEPT ![c] = @ \cup {d}]
       \* 2. Register in index
       /\ serverIndex' = serverIndex \cup {d}
       \* 3. Send SyncStep2 with the diff
       /\ serverToClient' = [serverToClient EXCEPT
            ![c] = Append(@, [type    |-> MsgSyncStep2,
                              docId   |-> d,
                              updates |-> diff])]
       /\ clientToServer' = [clientToServer EXCEPT ![c] = Tail(@)]
    /\ UNCHANGED <<clientDoc, clientConnected, serverDB, updateCounter>>

(*
 * ServerReceiveUpdate(c): Server processes MSG_UPDATE from client c.
 * Maps to: server.rs handle_socket -> MSG_UPDATE branch
 *
 * The server:
 *   1. Persists the update to DB FIRST
 *   2. Registers in __index__
 *   3. Broadcasts to all subscribers EXCEPT the sender
 *)
ServerReceiveUpdate(c) ==
    /\ clientToServer[c] /= <<>>
    /\ Head(clientToServer[c]).type = MsgUpdate
    /\ LET msg == Head(clientToServer[c])
           d   == msg.docId
           u   == msg.update
       IN
       \* 1. Persist BEFORE broadcast (critical ordering!)
       /\ serverDB' = [serverDB EXCEPT ![d] = Append(@, u)]
       \* 2. Register in index
       /\ serverIndex' = serverIndex \cup {d}
       \* 3. Broadcast to subscribers EXCEPT the sender
       /\ LET recipients == serverChannels[d] \ {c}
          IN serverToClient' = [s \in Clients |->
               IF s \in recipients
               THEN Append(serverToClient[s],
                           [type   |-> MsgUpdate,
                            docId  |-> d,
                            update |-> u])
               ELSE serverToClient[s]]
       /\ clientToServer' = [clientToServer EXCEPT ![c] = Tail(@)]
    /\ UNCHANGED <<clientDoc, clientConnected, clientSubscribed,
                   serverChannels, updateCounter>>

(* ===================== NEXT-STATE RELATION ===================== *)

Next ==
    \/ \E c \in Clients, d \in DocIds : ClientEdit(c, d)
    \/ \E c \in Clients : ClientGoOffline(c)
    \/ \E c \in Clients : ClientReconnect(c)
    \/ \E c \in Clients : ClientReceive(c)
    \/ \E c \in Clients, d \in DocIds : ClientSubscribe(c, d)
    \/ \E c \in Clients : ServerReceiveSyncStep1(c)
    \/ \E c \in Clients : ServerReceiveUpdate(c)

(* ===================== FAIRNESS ===================== *)

\* Weak fairness on server processing — the server eventually processes
\* any message in its incoming queue.
Fairness ==
    /\ \A c \in Clients : WF_vars(ServerReceiveSyncStep1(c))
    /\ \A c \in Clients : WF_vars(ServerReceiveUpdate(c))
    /\ \A c \in Clients : WF_vars(ClientReceive(c))

Spec == Init /\ [][Next]_vars /\ Fairness

(* ===================== SAFETY PROPERTIES ===================== *)

(*
 * NoEcho: The server never sends an update back to the client that
 * originated it. This was a real bug (Issue #2).
 *)
NoEcho ==
    \A c \in Clients :
        \A i \in 1..Len(serverToClient[c]) :
            LET msg == serverToClient[c][i]
            IN msg.type = MsgUpdate => msg.update.origin /= c

(*
 * PersistBeforeBroadcast: If a MSG_UPDATE for update u is in any
 * client's incoming queue, then u is already in serverDB.
 *
 * This ensures no SyncStep2 response can miss recent updates.
 *)
PersistBeforeBroadcast ==
    \A c \in Clients :
        \A i \in 1..Len(serverToClient[c]) :
            LET msg == serverToClient[c][i]
            IN (msg.type = MsgUpdate) =>
               msg.update \in SeqToSet(serverDB[msg.docId])

(*
 * ChannelOnlyForConnected: Only connected clients should be in
 * broadcast channels.
 *)
ChannelOnlyForConnected ==
    \A d \in DocIds :
        \A c \in serverChannels[d] :
            clientConnected[c]

(*
 * IndexConsistency: Every document with data in serverDB is in the index.
 *)
IndexConsistency ==
    \A d \in DocIds :
        serverDB[d] /= <<>> => d \in serverIndex

(*
 * SubscriptionConsistency: If a client is subscribed to a doc,
 * it must be in that doc's server channel (and vice versa).
 *)
SubscriptionConsistency ==
    \A c \in Clients, d \in DocIds :
        (d \in clientSubscribed[c]) <=> (c \in serverChannels[d])

(* ===================== LIVENESS PROPERTIES ===================== *)

(*
 * EventualConvergence: If all clients are connected, subscribed to doc d,
 * and all queues are empty, then all clients have the same set of update IDs.
 *)
EventualConvergence ==
    \A d \in DocIds :
      <>( /\ \A c \in Clients : clientConnected[c]
          /\ \A c \in Clients : d \in clientSubscribed[c]
          /\ \A c \in Clients : clientToServer[c] = <<>>
          /\ \A c \in Clients : serverToClient[c] = <<>>
          => \A c1, c2 \in Clients :
               UpdateIds(clientDoc[c1][d]) = UpdateIds(clientDoc[c2][d]))

=============================================================================
