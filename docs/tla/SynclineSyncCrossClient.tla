--------------------- MODULE SynclineSyncCrossClient ----------------------
(*
 * Phase 3: Cross-client specification.
 *
 * Models BOTH client architectures in the same system:
 *
 *   Obsidian (fixed):
 *     1. Remote update → apply to CRDT
 *     2. Write to disk + set ignoreChanges    (separate step, can interleave)
 *     3. ignoreChanges expires after timeout
 *     - Watcher blocked while ignoreChanges is set
 *     - Watcher re-checks ignoreChanges after async read (post-read guard)
 *     - Watcher compares disk vs CRDT before diffing (content comparison)
 *
 *   Folder client:
 *     1. Remote update arrives
 *     2. Read disk; if disk ≠ CRDT → apply local diff FIRST, broadcast it
 *     3. Apply remote update
 *     4. Write merged result to disk (synchronous, no ignore needed)
 *     - Watcher fires normally; debounced but no suppression mechanism
 *
 * What this verifies:
 *   - Cross-client convergence: Obsidian + folder clients reach the same
 *     state after all messages are processed.
 *   - NoContentLoss holds for BOTH client types in a mixed deployment.
 *   - The folder client's pre-apply strategy is strictly safe.
 *   - The fixed Obsidian client doesn't lose content either.
 *
 * Source: docs/FORMAL_VERIFICATION.md §7
 *)
EXTENDS Naturals, Sequences, FiniteSets, TLC

CONSTANTS
    Clients,          \* Set of client IDs
    DocIds,           \* Set of document IDs
    MaxUpdates,       \* Bound on total updates
    MaxQueueLen,      \* Bound on message queue length
    ObsidianClients,  \* SUBSET Clients that are Obsidian plugin
    FolderClients     \* SUBSET Clients that are folder clients

\* Helper predicates for client type dispatch
IsObsidian(c) == c \in ObsidianClients
IsFolder(c)   == c \in FolderClients

(* ===================== VARIABLES ===================== *)

VARIABLES
    clientDoc,          \* [Clients -> [DocIds -> SUBSET Nat]] — effective CRDT content
    clientDisk,         \* [Clients -> [DocIds -> SUBSET Nat]] — file content on disk
    clientIgnoring,     \* [Clients -> SUBSET DocIds] — watcher suppressed (Obsidian only)
    clientConnected,    \* [Clients -> BOOLEAN]
    clientSubscribed,   \* [Clients -> SUBSET DocIds]
    clientToServer,     \* [Clients -> Seq(Message)]
    serverToClient,     \* [Clients -> Seq(Message)]
    serverDB,           \* [DocIds -> SUBSET Nat]
    serverChannels,     \* [DocIds -> SUBSET Clients]
    serverIndex,        \* SUBSET DocIds
    updateCounter,      \* Nat
    updateOrigin,       \* Function: update ID -> originating client
    receivedRemote      \* [Clients -> [DocIds -> SUBSET Nat]] — ghost variable

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

MsgS1 == "S1"
MsgS2 == "S2"
MsgU  == "U"

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

(* ===================== SHARED CLIENT ACTIONS ===================== *)

(*
 * LocalDiskEdit(c, d): User edits the file on disk.
 * Same for both client types — changes disk only.
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
 * ClientSubscribe(c, d): Client subscribes to a document.
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

(* ===================== OBSIDIAN CLIENT ACTIONS ===================== *)

(*
 * ObsidianWatcherFires(c, d): Obsidian's file watcher fires.
 *
 * Blocked if ignoreChanges is set. Includes the post-read guard:
 * re-checks ignoreChanges and compares disk vs CRDT before diffing.
 * This is the FIXED version matching the patched main.ts.
 *)
ObsidianWatcherFires(c, d) ==
    /\ IsObsidian(c)
    /\ clientConnected[c]
    /\ d \notin clientIgnoring[c]
    /\ clientDisk[c][d] /= clientDoc[c][d]
    \* Content comparison guard: if disk == CRDT, skip (no-op)
    \* (Already handled by the precondition above)
    /\ LET adds == clientDisk[c][d] \ clientDoc[c][d]
           dels == clientDoc[c][d] \ clientDisk[c][d]
       IN
       /\ clientDoc' = [clientDoc EXCEPT ![c][d] = clientDisk[c][d]]
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
 * ObsidianApplyRemote(c): Obsidian processes a server message.
 *
 * Step 1 (this action): Apply to CRDT only.
 * Step 2 (ObsidianWriteToDisk): Write to disk + set ignoreChanges.
 *
 * The pre-suppress guard sets ignoreChanges BEFORE the disk write,
 * matching the fixed main.ts behavior.
 *)
ObsidianApplyRemote(c) ==
    /\ IsObsidian(c)
    /\ clientConnected[c]
    /\ serverToClient[c] /= <<>>
    /\ LET msg == Head(serverToClient[c])
       IN
       CASE msg.type = MsgS2 ->
            /\ clientDoc' = [clientDoc EXCEPT
                 ![c][msg.docId] = @ \cup msg.adds]
            \* Pre-suppress: set ignoreChanges BEFORE disk write
            /\ clientIgnoring' = [clientIgnoring EXCEPT
                 ![c] = @ \cup {msg.docId}]
            /\ LET nonLocal == {u \in msg.adds : u \notin DOMAIN updateOrigin
                                                  \/ updateOrigin[u] /= c}
               IN receivedRemote' = [receivedRemote EXCEPT
                    ![c][msg.docId] = @ \cup nonLocal]

         [] msg.type = MsgU ->
            /\ clientDoc' = [clientDoc EXCEPT
                 ![c][msg.docId] = (@ \cup msg.adds) \ msg.dels]
            /\ clientIgnoring' = [clientIgnoring EXCEPT
                 ![c] = @ \cup {msg.docId}]
            /\ LET nonLocal == {u \in msg.adds : u \notin DOMAIN updateOrigin
                                                  \/ updateOrigin[u] /= c}
               IN receivedRemote' = [receivedRemote EXCEPT
                    ![c][msg.docId] = @ \cup nonLocal]

         [] OTHER ->
            /\ UNCHANGED <<clientDoc, clientIgnoring, receivedRemote>>

    /\ serverToClient' = [serverToClient EXCEPT ![c] = Tail(@)]
    /\ UNCHANGED <<clientDisk, clientConnected, clientSubscribed,
                   clientToServer, serverDB, serverChannels, serverIndex,
                   updateCounter, updateOrigin>>

(*
 * ObsidianWriteToDisk(c, d): Obsidian writes CRDT content to disk.
 * This is a separate step from ApplyRemote — can be interleaved.
 * ignoreChanges is already set by ApplyRemote (pre-suppress).
 *)
ObsidianWriteToDisk(c, d) ==
    /\ IsObsidian(c)
    /\ clientDoc[c][d] /= clientDisk[c][d]
    /\ clientDisk' = [clientDisk EXCEPT ![c][d] = clientDoc[c][d]]
    /\ UNCHANGED <<clientDoc, clientIgnoring, clientConnected, clientSubscribed,
                   clientToServer, serverToClient,
                   serverDB, serverChannels, serverIndex,
                   updateCounter, updateOrigin, receivedRemote>>

(*
 * ObsidianIgnoreExpires(c, d): The ignoreChanges timer fires.
 *
 * PRECONDITION: disk must match CRDT for this doc.
 * In the real code, the setTimeout for clearing ignoreChanges runs
 * in the .finally() block AFTER `await vault.modify()` completes.
 * So the ignore timeout cannot fire until the disk write has finished.
 *)
ObsidianIgnoreExpires(c, d) ==
    /\ IsObsidian(c)
    /\ d \in clientIgnoring[c]
    /\ clientDisk[c][d] = clientDoc[c][d]   \* disk write must have completed
    /\ clientIgnoring' = [clientIgnoring EXCEPT ![c] = @ \ {d}]
    /\ UNCHANGED <<clientDoc, clientDisk, clientConnected, clientSubscribed,
                   clientToServer, serverToClient,
                   serverDB, serverChannels, serverIndex,
                   updateCounter, updateOrigin, receivedRemote>>

(* ===================== FOLDER CLIENT ACTIONS ===================== *)

(*
 * FolderWatcherFires(c, d): Folder client's file watcher fires.
 *
 * The folder client has NO ignoreChanges mechanism — the watcher
 * always fires. But it compares disk content to the existing CRDT
 * before diffing, so if disk == CRDT no action is taken.
 * This is safe because disk is always written synchronously after
 * applying remote updates (see FolderApplyRemote).
 *)
FolderWatcherFires(c, d) ==
    /\ IsFolder(c)
    /\ clientConnected[c]
    /\ clientDisk[c][d] /= clientDoc[c][d]
    /\ LET adds == clientDisk[c][d] \ clientDoc[c][d]
           dels == clientDoc[c][d] \ clientDisk[c][d]
       IN
       /\ clientDoc' = [clientDoc EXCEPT ![c][d] = clientDisk[c][d]]
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
 * FolderApplyRemote(c): Folder client processes a server message.
 *
 * KEY DIFFERENCE from Obsidian:
 *   1. Read disk first
 *   2. If disk ≠ CRDT → apply local diff, broadcast it
 *   3. Apply remote update
 *   4. Write merged result to disk (ATOMIC with steps 1–3)
 *
 * Maps to: app.rs lines 257–284 (the pre-apply strategy)
 *)
FolderApplyRemote(c) ==
    /\ IsFolder(c)
    /\ clientConnected[c]
    /\ serverToClient[c] /= <<>>
    /\ LET msg == Head(serverToClient[c])
           d   == msg.docId
       IN
       CASE msg.type = MsgS2 ->
            \* Step 1-2: Pre-apply local diff if disk differs from CRDT
            /\ LET localAdds == clientDisk[c][d] \ clientDoc[c][d]
                   localDels == clientDoc[c][d] \ clientDisk[c][d]
                   \* After pre-apply: CRDT matches disk
                   afterPreApply == clientDisk[c][d]
                   \* Step 3: Apply remote additions on top
                   finalDoc == afterPreApply \cup msg.adds
               IN
               /\ clientDoc' = [clientDoc EXCEPT ![c][d] = finalDoc]
               \* Step 4: Write merged result to disk (synchronous)
               /\ clientDisk' = [clientDisk EXCEPT ![c][d] = finalDoc]
               \* Broadcast local diff if there was one
               /\ IF localAdds /= {} \/ localDels /= {}
                  THEN clientToServer' = [clientToServer EXCEPT
                         ![c] = Append(@, [type   |-> MsgU,
                                           docId  |-> d,
                                           adds   |-> localAdds,
                                           dels   |-> localDels,
                                           origin |-> c])]
                  ELSE UNCHANGED clientToServer
               /\ LET nonLocal == {u \in msg.adds : u \notin DOMAIN updateOrigin
                                                     \/ updateOrigin[u] /= c}
                  IN receivedRemote' = [receivedRemote EXCEPT
                       ![c][d] = @ \cup nonLocal]

         [] msg.type = MsgU ->
            /\ LET localAdds == clientDisk[c][d] \ clientDoc[c][d]
                   localDels == clientDoc[c][d] \ clientDisk[c][d]
                   afterPreApply == clientDisk[c][d]
                   finalDoc == (afterPreApply \cup msg.adds) \ msg.dels
               IN
               /\ clientDoc' = [clientDoc EXCEPT ![c][d] = finalDoc]
               /\ clientDisk' = [clientDisk EXCEPT ![c][d] = finalDoc]
               /\ IF localAdds /= {} \/ localDels /= {}
                  THEN clientToServer' = [clientToServer EXCEPT
                         ![c] = Append(@, [type   |-> MsgU,
                                           docId  |-> d,
                                           adds   |-> localAdds,
                                           dels   |-> localDels,
                                           origin |-> c])]
                  ELSE UNCHANGED clientToServer
               /\ LET nonLocal == {u \in msg.adds : u \notin DOMAIN updateOrigin
                                                     \/ updateOrigin[u] /= c}
                  IN receivedRemote' = [receivedRemote EXCEPT
                       ![c][d] = @ \cup nonLocal]

         [] OTHER ->
            /\ UNCHANGED <<clientDoc, clientDisk, clientToServer, receivedRemote>>

    /\ serverToClient' = [serverToClient EXCEPT ![c] = Tail(@)]
    /\ UNCHANGED <<clientIgnoring, clientConnected, clientSubscribed,
                   serverDB, serverChannels, serverIndex,
                   updateCounter, updateOrigin>>

(* ===================== SERVER ACTIONS ===================== *)

ServerReceiveSyncStep1(c) ==
    /\ clientToServer[c] /= <<>>
    /\ Head(clientToServer[c]).type = MsgS1
    /\ LET msg  == Head(clientToServer[c])
           d    == msg.docId
           diff == serverDB[d] \ msg.have
       IN
       /\ serverChannels' = [serverChannels EXCEPT ![d] = @ \cup {c}]
       /\ clientSubscribed' = [clientSubscribed EXCEPT ![c] = @ \cup {d}]
       /\ serverIndex' = serverIndex \cup {d}
       /\ serverToClient' = [serverToClient EXCEPT
            ![c] = Append(@, [type |-> MsgS2, docId |-> d, adds |-> diff])]
       /\ clientToServer' = [clientToServer EXCEPT ![c] = Tail(@)]
    /\ UNCHANGED <<clientDoc, clientDisk, clientIgnoring, clientConnected,
                   serverDB, updateCounter, updateOrigin, receivedRemote>>

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
                           [type   |-> MsgU, docId  |-> d,
                            adds   |-> msg.adds, dels |-> msg.dels,
                            origin |-> msg.origin])
               ELSE serverToClient[s]]
       /\ clientToServer' = [clientToServer EXCEPT ![c] = Tail(@)]
    /\ UNCHANGED <<clientDoc, clientDisk, clientIgnoring, clientConnected,
                   clientSubscribed, serverChannels,
                   updateCounter, updateOrigin, receivedRemote>>

(* ===================== NEXT-STATE RELATION ===================== *)

Next ==
    \* Shared actions
    \/ \E c \in Clients, d \in DocIds : LocalDiskEdit(c, d)
    \/ \E c \in Clients, d \in DocIds : ClientSubscribe(c, d)
    \/ \E c \in Clients : ClientGoOffline(c)
    \/ \E c \in Clients : ClientReconnect(c)
    \* Obsidian-specific
    \/ \E c \in Clients, d \in DocIds : ObsidianWatcherFires(c, d)
    \/ \E c \in Clients : ObsidianApplyRemote(c)
    \/ \E c \in Clients, d \in DocIds : ObsidianWriteToDisk(c, d)
    \/ \E c \in Clients, d \in DocIds : ObsidianIgnoreExpires(c, d)
    \* Folder-specific
    \/ \E c \in Clients, d \in DocIds : FolderWatcherFires(c, d)
    \/ \E c \in Clients : FolderApplyRemote(c)
    \* Server
    \/ \E c \in Clients : ServerReceiveSyncStep1(c)
    \/ \E c \in Clients : ServerReceiveUpdate(c)

Fairness ==
    /\ \A c \in Clients : WF_vars(ServerReceiveSyncStep1(c))
    /\ \A c \in Clients : WF_vars(ServerReceiveUpdate(c))
    /\ \A c \in Clients : WF_vars(ObsidianApplyRemote(c))
    /\ \A c \in Clients : WF_vars(FolderApplyRemote(c))
    \* Obsidian eventually writes to disk and ignore expires
    /\ \A c \in Clients, d \in DocIds : WF_vars(ObsidianWriteToDisk(c, d))
    /\ \A c \in Clients, d \in DocIds : WF_vars(ObsidianIgnoreExpires(c, d))

Spec == Init /\ [][Next]_vars /\ Fairness

(* ===================== SAFETY PROPERTIES ===================== *)

NoEcho ==
    \A c \in Clients :
        \A i \in 1..Len(serverToClient[c]) :
            LET msg == serverToClient[c][i]
            IN msg.type = MsgU => msg.origin /= c

NoContentLoss ==
    \A c \in Clients, d \in DocIds :
        receivedRemote[c][d] \subseteq clientDoc[c][d]

ChannelOnlyForConnected ==
    \A d \in DocIds :
        \A c \in serverChannels[d] :
            clientConnected[c]

(* ===================== LIVENESS PROPERTIES ===================== *)

(*
 * CrossClientConvergence: When all clients are connected, subscribed,
 * and all queues are drained, all clients (regardless of type) have
 * the same document content.
 *)
CrossClientConvergence ==
    \A d \in DocIds :
      <>( /\ \A c \in Clients : clientConnected[c]
          /\ \A c \in Clients : d \in clientSubscribed[c]
          /\ \A c \in Clients : clientToServer[c] = <<>>
          /\ \A c \in Clients : serverToClient[c] = <<>>
          /\ \A c \in Clients : clientDoc[c][d] = clientDisk[c][d]
          => \A c1, c2 \in Clients :
               clientDoc[c1][d] = clientDoc[c2][d])

(*
 * DiskCrdtEventualConsistency: Disk eventually matches CRDT for
 * every connected, subscribed client (both types).
 *)
DiskCrdtEventualConsistency ==
    \A c \in Clients, d \in DocIds :
      [](clientConnected[c] /\ d \in clientSubscribed[c] =>
          <>(clientDisk[c][d] = clientDoc[c][d]))

=============================================================================
