----------------------- MODULE SynclineSyncDiffLayer -----------------------
(*
 * Phase 2: Diff Layer specification.
 *
 * Extends the core protocol with the file-system diff layer that converts
 * between disk files and CRDT operations. This models:
 *   - clientDisk: what's written on the physical file
 *   - clientIgnoring: Obsidian's ignoreChanges suppression
 *   - The stale-read bug (Issue #6) where a diff on stale disk content
 *     generates spurious DELETE operations
 *
 * KEY ABSTRACTION: Documents are modeled as SETS of update IDs.
 * CRDT merge = set union. The diff operation can both ADD and DELETE
 * items. Deletions propagate via messages carrying {adds, dels}.
 *
 * EXPECTED RESULT: The NoContentLoss invariant SHOULD BE VIOLATED,
 * confirming Issue #6 exists in the model. TLC will produce a
 * counterexample trace showing the exact bug scenario.
 *)
EXTENDS Naturals, Sequences, FiniteSets, TLC

CONSTANTS
    Clients,
    DocIds,
    MaxUpdates,
    MaxQueueLen

(* ===================== VARIABLES ===================== *)

VARIABLES
    clientDoc,          \* [Clients -> [DocIds -> SUBSET Nat]] — effective CRDT content
    clientDisk,         \* [Clients -> [DocIds -> SUBSET Nat]] — file content on disk
    clientIgnoring,     \* [Clients -> SUBSET DocIds] — watcher suppressed
    clientConnected,    \* [Clients -> BOOLEAN]
    clientSubscribed,   \* [Clients -> SUBSET DocIds]
    clientToServer,     \* [Clients -> Seq(Message)]
    serverToClient,     \* [Clients -> Seq(Message)]
    serverDB,           \* [DocIds -> SUBSET Nat] — server's merged doc state
    serverChannels,     \* [DocIds -> SUBSET Clients]
    serverIndex,        \* SUBSET DocIds
    updateCounter,      \* Nat
    updateOrigin,       \* Function: update ID -> originating client
    receivedRemote      \* [Clients -> [DocIds -> SUBSET Nat]] — ghost: non-local updates ever received

vars == <<clientDoc, clientDisk, clientIgnoring,
          clientConnected, clientSubscribed,
          clientToServer, serverToClient,
          serverDB, serverChannels, serverIndex,
          updateCounter, updateOrigin, receivedRemote>>

(* ===================== STATE CONSTRAINT ===================== *)

StateConstraint ==
    /\ \A c \in Clients : Len(clientToServer[c]) <= MaxQueueLen
    /\ \A c \in Clients : Len(serverToClient[c]) <= MaxQueueLen
    /\ updateCounter <= MaxUpdates

(* ===================== MESSAGE TYPES ===================== *)

MsgS1 == "S1"   \* SyncStep1: client -> server, carries state vector
MsgS2 == "S2"   \* SyncStep2: server -> client, carries missing updates (adds only)
MsgU  == "U"    \* Update: carries adds and dels

(* ===================== INITIAL STATE ===================== *)

Init ==
    /\ clientDoc        = [c \in Clients |-> [d \in DocIds |-> {}]]
    /\ clientDisk       = [c \in Clients |-> [d \in DocIds |-> {}]]
    /\ clientIgnoring   = [c \in Clients |-> {}]
    /\ clientConnected  = [c \in Clients |-> TRUE]
    /\ clientSubscribed = [c \in Clients |-> {}]
    /\ clientToServer   = [c \in Clients |-> <<>>]
    /\ serverToClient   = [c \in Clients |-> <<>>]
    /\ serverDB         = [d \in DocIds  |-> {}]
    /\ serverChannels   = [d \in DocIds  |-> {}]
    /\ serverIndex      = {}
    /\ updateCounter    = 0
    /\ updateOrigin     = <<>>
    /\ receivedRemote   = [c \in Clients |-> [d \in DocIds |-> {}]]

(* ===================== CLIENT ACTIONS ===================== *)

(*
 * LocalDiskEdit(c, d): User edits the file on disk (types in editor).
 * Changes disk ONLY. The watcher will later pick up the change.
 *)
LocalDiskEdit(c, d) ==
    /\ updateCounter < MaxUpdates
    /\ LET uid == updateCounter + 1
       IN
       /\ clientDisk' = [clientDisk EXCEPT ![c][d] = @ \cup {uid}]
       /\ updateCounter' = uid
       /\ updateOrigin' = [i \in DOMAIN updateOrigin \cup {uid} |->
            IF i = uid THEN c ELSE updateOrigin[i]]
    /\ UNCHANGED <<clientDoc, clientIgnoring, clientConnected, clientSubscribed,
                   clientToServer, serverToClient,
                   serverDB, serverChannels, serverIndex, receivedRemote>>

(*
 * ClientWatcherFires(c, d): File watcher detects change, diffs disk vs CRDT.
 *
 * After this action: clientDoc[c][d] = clientDisk[c][d].
 * Generates adds (local content) and dels (SPURIOUS if disk is stale!).
 * Blocked if ignoreChanges is set.
 *)
ClientWatcherFires(c, d) ==
    /\ clientConnected[c]
    /\ d \notin clientIgnoring[c]
    /\ clientDisk[c][d] /= clientDoc[c][d]
    /\ LET adds == clientDisk[c][d] \ clientDoc[c][d]
           dels == clientDoc[c][d] \ clientDisk[c][d]
       IN
       \* CRDT becomes disk content (the diff's effect)
       /\ clientDoc' = [clientDoc EXCEPT ![c][d] = clientDisk[c][d]]
       \* Send update to server if subscribed
       /\ IF d \in clientSubscribed[c]
          THEN clientToServer' = [clientToServer EXCEPT
                 ![c] = Append(@, [type   |-> MsgU,
                                   docId  |-> d,
                                   adds   |-> adds,
                                   dels   |-> dels,
                                   origin |-> c])]
          ELSE UNCHANGED clientToServer
    /\ UNCHANGED <<clientDisk, clientIgnoring, clientConnected, clientSubscribed,
                   serverToClient, serverDB, serverChannels, serverIndex,
                   updateCounter, updateOrigin, receivedRemote>>

(*
 * ClientSubscribe(c, d): Client sends SyncStep1 for doc d.
 *)
ClientSubscribe(c, d) ==
    /\ clientConnected[c]
    /\ d \notin clientSubscribed[c]
    /\ clientToServer' = [clientToServer EXCEPT
         ![c] = Append(@, [type  |-> MsgS1,
                           docId |-> d,
                           have  |-> clientDoc[c][d]])]
    /\ UNCHANGED <<clientDoc, clientDisk, clientIgnoring,
                   clientConnected, clientSubscribed,
                   serverToClient, serverDB, serverChannels,
                   serverIndex, updateCounter, updateOrigin, receivedRemote>>

(*
 * ClientApplyRemote(c): Process next message from server.
 * Applies to CRDT only — disk write is a SEPARATE action.
 * This models the real system where apply_update and vault.modify
 * are separate (potentially interleaved) steps.
 *)
ClientApplyRemote(c) ==
    /\ clientConnected[c]
    /\ serverToClient[c] /= <<>>
    /\ LET msg == Head(serverToClient[c])
       IN
       CASE msg.type = MsgS2 ->
            \* SyncStep2: merge additions into CRDT
            /\ clientDoc' = [clientDoc EXCEPT
                 ![c][msg.docId] = @ \cup msg.adds]
            /\ LET nonLocal == {u \in msg.adds : u \notin DOMAIN updateOrigin
                                                  \/ updateOrigin[u] /= c}
               IN receivedRemote' = [receivedRemote EXCEPT
                    ![c][msg.docId] = @ \cup nonLocal]

         [] msg.type = MsgU ->
            \* Update: apply adds and dels
            /\ clientDoc' = [clientDoc EXCEPT
                 ![c][msg.docId] = (@ \cup msg.adds) \ msg.dels]
            /\ LET nonLocal == {u \in msg.adds : u \notin DOMAIN updateOrigin
                                                  \/ updateOrigin[u] /= c}
               IN receivedRemote' = [receivedRemote EXCEPT
                    ![c][msg.docId] = @ \cup nonLocal]

         [] OTHER ->
            /\ UNCHANGED clientDoc
            /\ UNCHANGED receivedRemote

    /\ serverToClient' = [serverToClient EXCEPT ![c] = Tail(@)]
    /\ UNCHANGED <<clientDisk, clientIgnoring, clientConnected, clientSubscribed,
                   clientToServer, serverDB, serverChannels, serverIndex,
                   updateCounter, updateOrigin>>

(*
 * ClientWriteToDisk(c, d): Write CRDT content to disk file.
 * Sets ignoreChanges to suppress the file-watcher echo.
 *)
ClientWriteToDisk(c, d) ==
    /\ clientDoc[c][d] /= clientDisk[c][d]
    /\ clientDisk' = [clientDisk EXCEPT ![c][d] = clientDoc[c][d]]
    /\ clientIgnoring' = [clientIgnoring EXCEPT ![c] = @ \cup {d}]
    /\ UNCHANGED <<clientDoc, clientConnected, clientSubscribed,
                   clientToServer, serverToClient,
                   serverDB, serverChannels, serverIndex,
                   updateCounter, updateOrigin, receivedRemote>>

(*
 * ClientIgnoreExpires(c, d): The ignoreChanges timeout fires.
 * In Obsidian: setTimeout(() => ignoreChanges.delete(path), 500)
 *)
ClientIgnoreExpires(c, d) ==
    /\ d \in clientIgnoring[c]
    /\ clientIgnoring' = [clientIgnoring EXCEPT ![c] = @ \ {d}]
    /\ UNCHANGED <<clientDoc, clientDisk, clientConnected, clientSubscribed,
                   clientToServer, serverToClient,
                   serverDB, serverChannels, serverIndex,
                   updateCounter, updateOrigin, receivedRemote>>

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
    /\ UNCHANGED <<clientDoc, clientDisk, clientIgnoring,
                   serverDB, serverIndex, updateCounter,
                   updateOrigin, receivedRemote>>

(*
 * ClientReconnect(c): Client comes back online.
 *)
ClientReconnect(c) ==
    /\ ~clientConnected[c]
    /\ clientConnected' = [clientConnected EXCEPT ![c] = TRUE]
    /\ UNCHANGED <<clientDoc, clientDisk, clientIgnoring, clientSubscribed,
                   clientToServer, serverToClient, serverDB, serverChannels,
                   serverIndex, updateCounter, updateOrigin, receivedRemote>>

(* ===================== SERVER ACTIONS ===================== *)

(*
 * ServerReceiveSyncStep1(c): Process MSG_SYNC_STEP_1.
 *)
ServerReceiveSyncStep1(c) ==
    /\ clientToServer[c] /= <<>>
    /\ Head(clientToServer[c]).type = MsgS1
    /\ LET msg  == Head(clientToServer[c])
           d    == msg.docId
           diff == serverDB[d] \ msg.have   \* Updates the client is missing
       IN
       /\ serverChannels' = [serverChannels EXCEPT ![d] = @ \cup {c}]
       /\ clientSubscribed' = [clientSubscribed EXCEPT ![c] = @ \cup {d}]
       /\ serverIndex' = serverIndex \cup {d}
       /\ serverToClient' = [serverToClient EXCEPT
            ![c] = Append(@, [type |-> MsgS2, docId |-> d, adds |-> diff])]
       /\ clientToServer' = [clientToServer EXCEPT ![c] = Tail(@)]
    /\ UNCHANGED <<clientDoc, clientDisk, clientIgnoring, clientConnected,
                   serverDB, updateCounter, updateOrigin, receivedRemote>>

(*
 * ServerReceiveUpdate(c): Process MSG_UPDATE.
 * 1. Apply to server DB (persist)
 * 2. Broadcast to all subscribers EXCEPT sender
 *)
ServerReceiveUpdate(c) ==
    /\ clientToServer[c] /= <<>>
    /\ Head(clientToServer[c]).type = MsgU
    /\ LET msg == Head(clientToServer[c])
           d   == msg.docId
       IN
       /\ serverDB' = [serverDB EXCEPT ![d] = (@ \cup msg.adds) \ msg.dels]
       /\ serverIndex' = serverIndex \cup {d}
       /\ LET recipients == serverChannels[d] \ {c}
          IN serverToClient' = [s \in Clients |->
               IF s \in recipients
               THEN Append(serverToClient[s],
                           [type   |-> MsgU,
                            docId  |-> d,
                            adds   |-> msg.adds,
                            dels   |-> msg.dels,
                            origin |-> msg.origin])
               ELSE serverToClient[s]]
       /\ clientToServer' = [clientToServer EXCEPT ![c] = Tail(@)]
    /\ UNCHANGED <<clientDoc, clientDisk, clientIgnoring, clientConnected,
                   clientSubscribed, serverChannels,
                   updateCounter, updateOrigin, receivedRemote>>

(* ===================== NEXT-STATE RELATION ===================== *)

Next ==
    \/ \E c \in Clients, d \in DocIds : LocalDiskEdit(c, d)
    \/ \E c \in Clients, d \in DocIds : ClientWatcherFires(c, d)
    \/ \E c \in Clients, d \in DocIds : ClientSubscribe(c, d)
    \/ \E c \in Clients : ClientApplyRemote(c)
    \/ \E c \in Clients, d \in DocIds : ClientWriteToDisk(c, d)
    \/ \E c \in Clients, d \in DocIds : ClientIgnoreExpires(c, d)
    \/ \E c \in Clients : ClientGoOffline(c)
    \/ \E c \in Clients : ClientReconnect(c)
    \/ \E c \in Clients : ServerReceiveSyncStep1(c)
    \/ \E c \in Clients : ServerReceiveUpdate(c)

Fairness ==
    /\ \A c \in Clients : WF_vars(ServerReceiveSyncStep1(c))
    /\ \A c \in Clients : WF_vars(ServerReceiveUpdate(c))
    /\ \A c \in Clients : WF_vars(ClientApplyRemote(c))

Spec == Init /\ [][Next]_vars /\ Fairness

(* ===================== SAFETY PROPERTIES ===================== *)

(*
 * NoEcho: Server never sends update back to the originating client.
 *)
NoEcho ==
    \A c \in Clients :
        \A i \in 1..Len(serverToClient[c]) :
            LET msg == serverToClient[c][i]
            IN msg.type = MsgU => msg.origin /= c

(*
 * NoContentLoss: Once a non-local update is received and applied,
 * it should NEVER be removed from the CRDT.
 *
 * EXPECTED: VIOLATED — this is Issue #6. TLC will produce a
 * counterexample showing the stale-diff scenario.
 *)
NoContentLoss ==
    \A c \in Clients, d \in DocIds :
        receivedRemote[c][d] \subseteq clientDoc[c][d]

(*
 * ChannelOnlyForConnected: Only connected clients in broadcast channels.
 *)
ChannelOnlyForConnected ==
    \A d \in DocIds :
        \A c \in serverChannels[d] :
            clientConnected[c]

=============================================================================
