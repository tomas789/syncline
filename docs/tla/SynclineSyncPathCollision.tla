-------------------- MODULE SynclineSyncPathCollision ---------------------
(*
 * Phase 6: Path Collision specification.
 *
 * Models the scenario where two clients independently create a file
 * at the same path (e.g. "file.md") while offline, each assigning a
 * different UUID. When both reconnect, the system must:
 *   1. Detect the path collision
 *   2. Resolve it (server-wins: first UUID keeps canonical path)
 *   3. Ensure no content is lost (both docs end up on disk)
 *   4. Ensure no path duplication (at most one doc per path)
 *
 * Maps to: app.rs resolve_path_conflict (lines 640-719)
 *          app.rs collision detection (lines 297-338)
 *)
EXTENDS Naturals, Sequences, FiniteSets, TLC

CONSTANTS
    Clients,        \* {"A", "B"}
    DocIds,         \* {"d1", "d2"}
    MaxQueueLen

\* Paths: canonical + one conflict path per client
CanonPath == "file.md"
NullPath  == "none"

ConflictPathFor(c) ==
    CASE c = "A" -> "file_A.md"
      [] c = "B" -> "file_B.md"

AllPaths == {"file.md", "file_A.md", "file_B.md"}

(* ===================== VARIABLES ===================== *)

VARIABLES
    \* Per-client, per-doc state
    clientHasDoc,       \* [Clients -> [DocIds -> BOOLEAN]]
    clientDocPath,      \* [Clients -> [DocIds -> AllPaths \cup {NullPath}]]
    clientDiskDocAt,    \* [Clients -> [DocIds -> AllPaths \cup {NullPath}]]

    \* Server state
    serverHasDoc,       \* [DocIds -> BOOLEAN]
    serverDocPath,      \* [DocIds -> AllPaths \cup {NullPath}]
    serverIndex,        \* SUBSET DocIds
    serverChannels,     \* [DocIds -> SUBSET Clients]

    \* Collision-detection tracking
    freshlyCreated,     \* [Clients -> SUBSET DocIds]
    initialServerDocs,  \* [Clients -> SUBSET DocIds]

    \* Protocol
    clientConnected,    \* [Clients -> BOOLEAN]
    clientKnownDocs,    \* [Clients -> SUBSET DocIds]
    clientSubscribed,   \* [Clients -> SUBSET DocIds]
    clientToServer,     \* [Clients -> Seq(Message)]
    serverToClient      \* [Clients -> Seq(Message)]

vars == <<clientHasDoc, clientDocPath, clientDiskDocAt,
          serverHasDoc, serverDocPath, serverIndex, serverChannels,
          freshlyCreated, initialServerDocs,
          clientConnected, clientKnownDocs, clientSubscribed,
          clientToServer, serverToClient>>

(* ===================== STATE CONSTRAINT ===================== *)

StateConstraint ==
    /\ \A c \in Clients : Len(clientToServer[c]) <= MaxQueueLen
    /\ \A c \in Clients : Len(serverToClient[c]) <= MaxQueueLen

(* ===================== MESSAGE TYPES ===================== *)
MsgS1  == "S1"   \* SyncStep1 for content doc
MsgS2  == "S2"   \* SyncStep2 response
MsgU   == "U"    \* Update (path change)

(* ===================== INITIAL STATE ===================== *)

Init ==
    /\ clientHasDoc     = [c \in Clients |-> [d \in DocIds |-> FALSE]]
    /\ clientDocPath    = [c \in Clients |-> [d \in DocIds |-> NullPath]]
    /\ clientDiskDocAt  = [c \in Clients |-> [d \in DocIds |-> NullPath]]
    /\ serverHasDoc     = [d \in DocIds  |-> FALSE]
    /\ serverDocPath    = [d \in DocIds  |-> NullPath]
    /\ serverIndex      = {}
    /\ serverChannels   = [d \in DocIds  |-> {}]
    /\ freshlyCreated   = [c \in Clients |-> {}]
    /\ initialServerDocs = [c \in Clients |-> {}]
    /\ clientConnected  = [c \in Clients |-> FALSE]
    /\ clientKnownDocs  = [c \in Clients |-> {}]
    /\ clientSubscribed = [c \in Clients |-> {}]
    /\ clientToServer   = [c \in Clients |-> <<>>]
    /\ serverToClient   = [c \in Clients |-> <<>>]

(* ===================== CLIENT ACTIONS ===================== *)

(*
 * CreateDocOffline(c, d): Client c creates doc d at CanonPath while offline.
 * Assigns UUID (= d), sets meta.path = CanonPath, writes to disk.
 * Maps to: user creates file.md while offline; bootstrap_offline_changes
 *)
CreateDocOffline(c, d) ==
    /\ ~clientConnected[c]
    /\ ~clientHasDoc[c][d]
    \* No other doc already at CanonPath on this client's disk
    /\ \A d2 \in DocIds : clientDiskDocAt[c][d2] /= CanonPath
    /\ clientHasDoc'    = [clientHasDoc EXCEPT ![c][d] = TRUE]
    /\ clientDocPath'   = [clientDocPath EXCEPT ![c][d] = CanonPath]
    /\ clientDiskDocAt' = [clientDiskDocAt EXCEPT ![c][d] = CanonPath]
    /\ freshlyCreated'  = [freshlyCreated EXCEPT ![c] = @ \cup {d}]
    /\ UNCHANGED <<serverHasDoc, serverDocPath, serverIndex, serverChannels,
                   initialServerDocs, clientConnected, clientKnownDocs,
                   clientSubscribed, clientToServer, serverToClient>>

(*
 * ClientGoOnline(c): Client connects.
 * Captures initialServerDocs = current serverIndex (snapshot at connect time).
 * Maps to: app.rs — initial __index__ sync on connect
 *)
ClientGoOnline(c) ==
    /\ ~clientConnected[c]
    /\ clientConnected'   = [clientConnected EXCEPT ![c] = TRUE]
    /\ initialServerDocs' = [initialServerDocs EXCEPT ![c] = serverIndex]
    /\ UNCHANGED <<clientHasDoc, clientDocPath, clientDiskDocAt,
                   serverHasDoc, serverDocPath, serverIndex, serverChannels,
                   freshlyCreated, clientKnownDocs, clientSubscribed,
                   clientToServer, serverToClient>>

(*
 * ClientUploadDoc(c, d): Client uploads a locally-created doc to server.
 * Sends SyncStep1 with the doc's path.
 *)
ClientUploadDoc(c, d) ==
    /\ clientConnected[c]
    /\ clientHasDoc[c][d]
    /\ d \notin clientSubscribed[c]
    /\ clientToServer' = [clientToServer EXCEPT
         ![c] = Append(@, [type |-> MsgS1,
                           docId |-> d,
                           path |-> clientDocPath[c][d]])]
    /\ UNCHANGED <<clientHasDoc, clientDocPath, clientDiskDocAt,
                   serverHasDoc, serverDocPath, serverIndex, serverChannels,
                   freshlyCreated, initialServerDocs, clientConnected,
                   clientKnownDocs, clientSubscribed, serverToClient>>

(*
 * ClientDiscoverDoc(c, d): Client discovers doc d exists on server.
 * Abstraction of __index__ propagation (proven in Phase 4).
 *)
ClientDiscoverDoc(c, d) ==
    /\ clientConnected[c]
    /\ d \in serverIndex
    /\ d \notin clientKnownDocs[c]
    /\ clientKnownDocs' = [clientKnownDocs EXCEPT ![c] = @ \cup {d}]
    /\ UNCHANGED <<clientHasDoc, clientDocPath, clientDiskDocAt,
                   serverHasDoc, serverDocPath, serverIndex, serverChannels,
                   freshlyCreated, initialServerDocs, clientConnected,
                   clientSubscribed, clientToServer, serverToClient>>

(*
 * ClientSyncDoc(c, d): Client sends SyncStep1 for a discovered doc.
 *)
ClientSyncDoc(c, d) ==
    /\ clientConnected[c]
    /\ d \in clientKnownDocs[c]
    /\ d \notin clientSubscribed[c]
    /\ ~clientHasDoc[c][d]  \* Don't sync docs we already have (upload instead)
    /\ clientToServer' = [clientToServer EXCEPT
         ![c] = Append(@, [type |-> MsgS1,
                           docId |-> d,
                           path |-> NullPath])]
    /\ UNCHANGED <<clientHasDoc, clientDocPath, clientDiskDocAt,
                   serverHasDoc, serverDocPath, serverIndex, serverChannels,
                   freshlyCreated, initialServerDocs, clientConnected,
                   clientKnownDocs, clientSubscribed, serverToClient>>

(*
 * ClientApplyRemote(c): Client processes a message from the server.
 * This is where collision detection and resolution happens.
 *
 * Collision condition (from app.rs lines 297-318):
 *   1. Incoming doc's path matches a local doc's path
 *   2. Local doc was freshly created
 *   3. Incoming doc was in initialServerDocs
 *
 * Resolution (from app.rs lines 640-719):
 *   Server's UUID keeps canonical path
 *   Local UUID moved to conflict path
 *)
ClientApplyRemote(c) ==
    /\ clientConnected[c]
    /\ serverToClient[c] /= <<>>
    /\ LET msg == Head(serverToClient[c])
           d   == msg.docId
           incomingPath == msg.path
       IN
       CASE msg.type = MsgS2 ->
            \* FIXED: Find colliding docs using path_map only.
            \* Any freshly-created local doc at the same path triggers resolution,
            \* regardless of when the incoming doc appeared on the server.
            LET colliders == {d2 \in DocIds :
                  /\ d2 /= d
                  /\ clientDiskDocAt[c][d2] = incomingPath
                  /\ incomingPath /= NullPath
                  /\ d2 \in freshlyCreated[c]}
            IN
            IF colliders /= {}
            THEN
              \* COLLISION DETECTED — resolve: server wins, local gets conflict path
              LET collidingDoc == CHOOSE d2 \in colliders : TRUE
                  conflictPath == ConflictPathFor(c)
              IN
              /\ clientHasDoc'    = [clientHasDoc EXCEPT ![c][d] = TRUE]
              /\ clientDocPath'   = [clientDocPath EXCEPT
                   ![c][d] = incomingPath,
                   ![c][collidingDoc] = conflictPath]
              /\ clientDiskDocAt' = [clientDiskDocAt EXCEPT
                   ![c][d] = incomingPath,
                   ![c][collidingDoc] = conflictPath]
              /\ clientSubscribed' = [clientSubscribed EXCEPT ![c] = @ \cup {d}]
              /\ freshlyCreated'  = [freshlyCreated EXCEPT
                   ![c] = @ \ {collidingDoc}]
              \* Broadcast the conflict resolution (local doc path changed)
              /\ clientToServer'  = [clientToServer EXCEPT
                   ![c] = Append(@, [type |-> MsgU,
                                     docId |-> collidingDoc,
                                     path |-> conflictPath,
                                     origin |-> c])]
              /\ UNCHANGED <<serverHasDoc, serverDocPath, serverIndex,
                             serverChannels, initialServerDocs,
                             clientConnected, clientKnownDocs>>
            ELSE
              \* No collision — normal apply.
              \* If we already have this doc with a local path (e.g. after
              \* conflict resolution), keep the local path — don't revert to
              \* a stale SyncStep2 echo.
              LET effectivePath == IF clientHasDoc[c][d]
                                      /\ clientDocPath[c][d] /= NullPath
                                   THEN clientDocPath[c][d]
                                   ELSE incomingPath
              IN
              /\ clientHasDoc'    = [clientHasDoc EXCEPT ![c][d] = TRUE]
              /\ clientDocPath'   = [clientDocPath EXCEPT ![c][d] = effectivePath]
              /\ clientDiskDocAt' = [clientDiskDocAt EXCEPT ![c][d] = effectivePath]
              /\ clientSubscribed' = [clientSubscribed EXCEPT ![c] = @ \cup {d}]
              /\ UNCHANGED <<freshlyCreated, clientToServer,
                             serverHasDoc, serverDocPath, serverIndex,
                             serverChannels, initialServerDocs,
                             clientConnected, clientKnownDocs>>

         [] msg.type = MsgU ->
            \* Path update from another client (e.g. conflict resolution broadcast)
            LET d2 == msg.docId
                newPath == msg.path
            IN
            IF clientHasDoc[c][d2]
            THEN
              /\ clientDocPath'   = [clientDocPath EXCEPT ![c][d2] = newPath]
              /\ clientDiskDocAt' = [clientDiskDocAt EXCEPT ![c][d2] = newPath]
              /\ UNCHANGED <<clientHasDoc, freshlyCreated, clientToServer,
                             clientSubscribed, serverHasDoc, serverDocPath,
                             serverIndex, serverChannels, initialServerDocs,
                             clientConnected, clientKnownDocs>>
            ELSE
              \* Don't have this doc yet — apply creates it
              /\ clientHasDoc'    = [clientHasDoc EXCEPT ![c][d2] = TRUE]
              /\ clientDocPath'   = [clientDocPath EXCEPT ![c][d2] = newPath]
              /\ clientDiskDocAt' = [clientDiskDocAt EXCEPT ![c][d2] = newPath]
              /\ UNCHANGED <<freshlyCreated, clientToServer, clientSubscribed,
                             serverHasDoc, serverDocPath, serverIndex,
                             serverChannels, initialServerDocs,
                             clientConnected, clientKnownDocs>>

         [] OTHER ->
            UNCHANGED <<clientHasDoc, clientDocPath, clientDiskDocAt,
                        freshlyCreated, clientToServer, clientSubscribed,
                        serverHasDoc, serverDocPath, serverIndex,
                        serverChannels, initialServerDocs,
                        clientConnected, clientKnownDocs>>

    /\ serverToClient' = [serverToClient EXCEPT ![c] = Tail(@)]

(*
 * ClientGoOffline(c): Client disconnects.
 *)
ClientGoOffline(c) ==
    /\ clientConnected[c]
    /\ clientConnected'  = [clientConnected EXCEPT ![c] = FALSE]
    /\ clientSubscribed' = [clientSubscribed EXCEPT ![c] = {}]
    /\ serverChannels'   = [d \in DocIds |-> serverChannels[d] \ {c}]
    /\ clientToServer'   = [clientToServer EXCEPT ![c] = <<>>]
    /\ serverToClient'   = [serverToClient EXCEPT ![c] = <<>>]
    /\ UNCHANGED <<clientHasDoc, clientDocPath, clientDiskDocAt,
                   serverHasDoc, serverDocPath, serverIndex,
                   freshlyCreated, initialServerDocs, clientKnownDocs>>

(* ===================== SERVER ACTIONS ===================== *)

(*
 * ServerReceiveSyncStep1(c): Server processes SyncStep1.
 * Registers doc in index, subscribes client, sends back data.
 * If doc is new (from upload), stores the path.
 *)
ServerReceiveSyncStep1(c) ==
    /\ clientToServer[c] /= <<>>
    /\ Head(clientToServer[c]).type = MsgS1
    /\ LET msg == Head(clientToServer[c])
           d   == msg.docId
           isNewDoc == ~serverHasDoc[d]
       IN
       /\ serverChannels'  = [serverChannels EXCEPT ![d] = @ \cup {c}]
       /\ clientSubscribed' = [clientSubscribed EXCEPT ![c] = @ \cup {d}]
       \* If upload (client has path), store it; register in index
       /\ serverHasDoc'    = [serverHasDoc EXCEPT ![d] = TRUE]
       /\ serverDocPath'   = [serverDocPath EXCEPT ![d] =
            IF msg.path /= NullPath /\ serverDocPath[d] = NullPath
            THEN msg.path ELSE serverDocPath[d]]
       /\ serverIndex'     = serverIndex \cup {d}
       \* Send SyncStep2 to requesting client with current server state
       /\ serverToClient'  = [s \in Clients |->
            IF s = c
            THEN Append(serverToClient[s],
                        [type  |-> MsgS2,
                         docId |-> d,
                         path  |-> IF msg.path /= NullPath
                                   THEN msg.path
                                   ELSE serverDocPath[d]])
            ELSE IF isNewDoc /\ s \in serverChannels[d] \ {c}
            THEN Append(serverToClient[s],
                        [type  |-> MsgS2,
                         docId |-> d,
                         path  |-> msg.path])
            ELSE serverToClient[s]]
       /\ clientToServer' = [clientToServer EXCEPT ![c] = Tail(@)]
    /\ UNCHANGED <<clientHasDoc, clientDocPath, clientDiskDocAt,
                   freshlyCreated, initialServerDocs, clientConnected,
                   clientKnownDocs>>

(*
 * ServerReceiveUpdate(c): Server processes a path update.
 * Broadcasts to other subscribers.
 *)
ServerReceiveUpdate(c) ==
    /\ clientToServer[c] /= <<>>
    /\ Head(clientToServer[c]).type = MsgU
    /\ LET msg == Head(clientToServer[c])
           d   == msg.docId
       IN
       /\ serverDocPath' = [serverDocPath EXCEPT ![d] = msg.path]
       /\ LET recipients == serverChannels[d] \ {c}
          IN serverToClient' = [s \in Clients |->
               IF s \in recipients
               THEN Append(serverToClient[s],
                           [type  |-> MsgU,
                            docId |-> d,
                            path  |-> msg.path,
                            origin |-> msg.origin])
               ELSE serverToClient[s]]
       /\ clientToServer' = [clientToServer EXCEPT ![c] = Tail(@)]
    /\ UNCHANGED <<clientHasDoc, clientDocPath, clientDiskDocAt,
                   serverHasDoc, serverIndex, serverChannels,
                   freshlyCreated, initialServerDocs, clientConnected,
                   clientKnownDocs, clientSubscribed>>

(* ===================== NEXT-STATE RELATION ===================== *)

Next ==
    \/ \E c \in Clients, d \in DocIds : CreateDocOffline(c, d)
    \/ \E c \in Clients : ClientGoOnline(c)
    \/ \E c \in Clients, d \in DocIds : ClientUploadDoc(c, d)
    \/ \E c \in Clients, d \in DocIds : ClientDiscoverDoc(c, d)
    \/ \E c \in Clients, d \in DocIds : ClientSyncDoc(c, d)
    \/ \E c \in Clients : ClientApplyRemote(c)
    \/ \E c \in Clients : ClientGoOffline(c)
    \/ \E c \in Clients : ServerReceiveSyncStep1(c)
    \/ \E c \in Clients : ServerReceiveUpdate(c)

Fairness ==
    /\ \A c \in Clients : WF_vars(ServerReceiveSyncStep1(c))
    /\ \A c \in Clients : WF_vars(ServerReceiveUpdate(c))
    /\ \A c \in Clients : WF_vars(ClientApplyRemote(c))
    /\ \A c \in Clients, d \in DocIds : WF_vars(ClientDiscoverDoc(c, d))
    /\ \A c \in Clients, d \in DocIds : WF_vars(ClientSyncDoc(c, d))
    /\ \A c \in Clients, d \in DocIds : WF_vars(ClientUploadDoc(c, d))

Spec == Init /\ [][Next]_vars /\ Fairness

(* ===================== SAFETY INVARIANTS ===================== *)

(*
 * NoPathDuplication: On each client's disk, at most one doc per path.
 * Two docs must never occupy the same disk path.
 *)
NoPathDuplication ==
    \A c \in Clients :
        \A d1, d2 \in DocIds :
            d1 /= d2 =>
                \/ clientDiskDocAt[c][d1] = NullPath
                \/ clientDiskDocAt[c][d2] = NullPath
                \/ clientDiskDocAt[c][d1] /= clientDiskDocAt[c][d2]

(*
 * NoContentLoss: If a doc was created and the client is connected
 * and subscribed, the doc must be on disk somewhere.
 *)
NoContentLoss ==
    \A c \in Clients, d \in DocIds :
        (clientHasDoc[c][d] /\ clientDocPath[c][d] /= NullPath)
        => clientDiskDocAt[c][d] /= NullPath

(*
 * ChannelOnlyForConnected: Only connected clients in channels.
 *)
ChannelOnlyForConnected ==
    \A d \in DocIds :
        \A c \in serverChannels[d] : clientConnected[c]

(* ===================== LIVENESS PROPERTIES ===================== *)

(*
 * CollisionConvergence: Eventually, all docs that exist on the server
 * are on disk on all connected clients, at non-colliding paths.
 *)
CollisionConvergence ==
    <>( /\ \A c \in Clients : clientConnected[c]
        /\ \A c \in Clients : clientToServer[c] = <<>>
        /\ \A c \in Clients : serverToClient[c] = <<>>
        => /\ \A c \in Clients, d \in DocIds :
                serverHasDoc[d] => clientDiskDocAt[c][d] /= NullPath
           /\ \A c \in Clients, d1, d2 \in DocIds :
                d1 /= d2 /\ clientDiskDocAt[c][d1] /= NullPath
                          /\ clientDiskDocAt[c][d2] /= NullPath
                => clientDiskDocAt[c][d1] /= clientDiskDocAt[c][d2])

=============================================================================
