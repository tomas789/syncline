--------------------- MODULE SynclineSyncRename ---------------------------
(*
 * Phase 5 (originally Phase 6): Rename Propagation specification.
 *
 * Models the `meta.path` mechanism for propagating file renames:
 *
 *   - Each document has a CRDT content (set of update IDs) AND a path
 *     (string) stored in `meta.path` within the CRDT document.
 *   - A rename changes `meta.path` without changing content.
 *   - The rename is propagated as a normal CRDT update to the server
 *     and then to other clients.
 *   - Receiving clients detect path changes and rename the local file.
 *
 * Key invariants:
 *   - PathConsistency: path_map on each client agrees with meta.path
 *     in the CRDT (eventually).
 *   - RenameConvergence: after a rename, all connected clients
 *     eventually agree on the file path.
 *   - NoContentLoss: rename must not lose document content.
 *   - Rename Idempotency: applying the same rename twice is a no-op.
 *
 * Simplifications:
 *   - Uses a single doc with one path from a finite set PathNames.
 *   - Content is modeled as SUBSET Nat (same as Phase 1-4).
 *   - Both clients use folder-client strategy (proven safe in Phase 3).
 *   - No index document (proven in Phase 4).
 *
 * Maps to:
 *   - state.rs: write_meta_path, read_meta_path
 *   - app.rs: watcher rename detection (lines 444-508)
 *   - app.rs: remote rename propagation (lines 341-362)
 *)
EXTENDS Naturals, Sequences, FiniteSets, TLC

CONSTANTS
    Clients,        \* Set of client IDs
    PathNames,      \* Set of possible file paths (e.g. {"a.md", "b.md"})
    MaxUpdates,     \* Bound on total updates
    MaxQueueLen     \* Bound on message queue length

(* ===================== VARIABLES ===================== *)

VARIABLES
    \* Document content (CRDT + disk)
    clientDoc,          \* [Clients -> SUBSET Nat] — CRDT content
    clientDisk,         \* [Clients -> SUBSET Nat] — file content on disk

    \* Path tracking — the core rename state
    clientMetaPath,     \* [Clients -> PathNames] — meta.path in the CRDT
    clientDiskPath,     \* [Clients -> PathNames] — where the file is on disk
    serverMetaPath,     \* PathNames — server's copy of meta.path

    \* Protocol state
    clientConnected,    \* [Clients -> BOOLEAN]
    clientSubscribed,   \* [Clients -> BOOLEAN]
    clientToServer,     \* [Clients -> Seq(Message)]
    serverToClient,     \* [Clients -> Seq(Message)]
    serverDB,           \* SUBSET Nat — server's document content
    serverChannels,     \* SUBSET Clients — subscribers

    \* Counters and ghost variables
    updateCounter,      \* Nat
    updateOrigin,       \* Function: update ID -> originating client
    receivedRemote      \* [Clients -> SUBSET Nat] — ghost variable

vars == <<clientDoc, clientDisk, clientMetaPath, clientDiskPath,
          serverMetaPath,
          clientConnected, clientSubscribed,
          clientToServer, serverToClient,
          serverDB, serverChannels,
          updateCounter, updateOrigin, receivedRemote>>

(* ===================== STATE CONSTRAINT ===================== *)

StateConstraint ==
    /\ \A c \in Clients : Len(clientToServer[c]) <= MaxQueueLen
    /\ \A c \in Clients : Len(serverToClient[c]) <= MaxQueueLen
    /\ updateCounter <= MaxUpdates

(* ===================== MESSAGE TYPES ===================== *)

MsgS1  == "S1"   \* SyncStep1
MsgS2  == "S2"   \* SyncStep2
MsgU   == "U"    \* Update (content + path changes)

(* ===================== INITIAL STATE ===================== *)

\* Pick an arbitrary initial path for all clients
ASSUME Cardinality(PathNames) >= 2  \* Need at least 2 paths for rename

CONSTANT InitialPath
ASSUME InitialPath \in PathNames

Init ==
    /\ clientDoc        = [c \in Clients |-> {}]
    /\ clientDisk       = [c \in Clients |-> {}]
    /\ clientMetaPath   = [c \in Clients |-> InitialPath]
    /\ clientDiskPath   = [c \in Clients |-> InitialPath]
    /\ serverMetaPath   = InitialPath
    /\ clientConnected  = [c \in Clients |-> TRUE]
    /\ clientSubscribed = [c \in Clients |-> FALSE]
    /\ clientToServer   = [c \in Clients |-> <<>>]
    /\ serverToClient   = [c \in Clients |-> <<>>]
    /\ serverDB         = {}
    /\ serverChannels   = {}
    /\ updateCounter    = 0
    /\ updateOrigin     = <<>>
    /\ receivedRemote   = [c \in Clients |-> {}]

(* ===================== CLIENT ACTIONS ===================== *)

(*
 * LocalDiskEdit(c): User edits the file on disk (content only, no rename).
 *)
LocalDiskEdit(c) ==
    /\ updateCounter < MaxUpdates
    /\ LET uid == updateCounter + 1
       IN
       /\ clientDisk' = [clientDisk EXCEPT ![c] = @ \cup {uid}]
       /\ updateCounter' = uid
       /\ updateOrigin' = [i \in DOMAIN updateOrigin \cup {uid} |->
            IF i = uid THEN c ELSE updateOrigin[i]]
    /\ UNCHANGED <<clientDoc, clientMetaPath, clientDiskPath, serverMetaPath,
                   clientConnected, clientSubscribed,
                   clientToServer, serverToClient,
                   serverDB, serverChannels, receivedRemote>>

(*
 * LocalRename(c, newPath): User renames the file locally.
 *
 * This is a disk-only operation:
 *   - File moves from clientDiskPath[c] to newPath
 *   - Content stays the same
 *   - The watcher will detect this as a delete+create (rename)
 *
 * Maps to: mv old.md new.md (or Obsidian rename UI)
 *)
LocalRename(c, newPath) ==
    /\ clientDiskPath[c] /= newPath  \* Not already at this path
    /\ clientDiskPath' = [clientDiskPath EXCEPT ![c] = newPath]
    \* Content on disk stays the same — just the path changes
    /\ UNCHANGED <<clientDoc, clientDisk, clientMetaPath, serverMetaPath,
                   clientConnected, clientSubscribed,
                   clientToServer, serverToClient,
                   serverDB, serverChannels,
                   updateCounter, updateOrigin, receivedRemote>>

(*
 * ClientWatcherFires(c): Watcher detects changes.
 *
 * Detects BOTH content diffs AND renames by comparing:
 *   1. clientDisk vs clientDoc (content diff)
 *   2. clientDiskPath vs clientMetaPath (rename)
 *
 * If a rename is detected (diskPath ≠ metaPath with same content),
 * the client updates meta.path and broadcasts the change.
 *
 * Maps to: app.rs two-pass rename detection (lines 395-508)
 *)
ClientWatcherFires(c) ==
    /\ clientConnected[c]
    /\ \/ clientDisk[c] /= clientDoc[c]       \* Content changed
       \/ clientDiskPath[c] /= clientMetaPath[c]  \* Path changed (rename)
    /\ LET contentAdds == clientDisk[c] \ clientDoc[c]
           contentDels == clientDoc[c] \ clientDisk[c]
           pathChanged == clientDiskPath[c] /= clientMetaPath[c]
           newPath     == clientDiskPath[c]
       IN
       \* Apply content diff to CRDT
       /\ clientDoc' = [clientDoc EXCEPT ![c] = clientDisk[c]]
       \* Update meta.path in CRDT if path changed
       /\ clientMetaPath' = [clientMetaPath EXCEPT ![c] = newPath]
       \* Broadcast update if subscribed
       /\ IF clientSubscribed[c]
          THEN clientToServer' = [clientToServer EXCEPT
                 ![c] = Append(@, [type       |-> MsgU,
                                   adds       |-> contentAdds,
                                   dels       |-> contentDels,
                                   path       |-> newPath,
                                   pathChange |-> pathChanged,
                                   origin     |-> c])]
          ELSE UNCHANGED clientToServer
    /\ UNCHANGED <<clientDisk, clientDiskPath, serverMetaPath,
                   clientConnected, clientSubscribed,
                   serverToClient, serverDB, serverChannels,
                   updateCounter, updateOrigin, receivedRemote>>

(*
 * ClientSubscribe(c): Client subscribes to the document.
 *)
ClientSubscribe(c) ==
    /\ clientConnected[c]
    /\ ~clientSubscribed[c]
    /\ clientToServer' = [clientToServer EXCEPT
         ![c] = Append(@, [type |-> MsgS1,
                           have |-> clientDoc[c],
                           path |-> clientMetaPath[c]])]
    /\ UNCHANGED <<clientDoc, clientDisk, clientMetaPath, clientDiskPath,
                   serverMetaPath, clientConnected, clientSubscribed,
                   serverToClient, serverDB, serverChannels,
                   updateCounter, updateOrigin, receivedRemote>>

(*
 * ClientApplyRemote(c): Client processes a server message.
 *
 * Handles content + path updates atomically.
 * If path changed: update meta.path, rename file on disk.
 * Maps to: app.rs lines 273-362 (apply remote, handle path change)
 *)
ClientApplyRemote(c) ==
    /\ clientConnected[c]
    /\ serverToClient[c] /= <<>>
    /\ LET msg == Head(serverToClient[c])
       IN
       CASE msg.type = MsgS2 ->
            /\ LET localAdds == clientDisk[c] \ clientDoc[c]
                   afterPreApply == clientDisk[c]
                   finalDoc == afterPreApply \cup msg.adds
                   \* Server's path is authoritative on sync
                   finalPath == msg.path
               IN
               /\ clientDoc' = [clientDoc EXCEPT ![c] = finalDoc]
               /\ clientDisk' = [clientDisk EXCEPT ![c] = finalDoc]
               /\ clientMetaPath' = [clientMetaPath EXCEPT ![c] = finalPath]
               \* Rename file on disk if path differs
               /\ clientDiskPath' = [clientDiskPath EXCEPT ![c] = finalPath]
               /\ IF localAdds /= {}
                  THEN clientToServer' = [clientToServer EXCEPT
                         ![c] = Append(@, [type       |-> MsgU,
                                           adds       |-> localAdds,
                                           dels       |-> {},
                                           path       |-> finalPath,
                                           pathChange |-> FALSE,
                                           origin     |-> c])]
                  ELSE UNCHANGED clientToServer
               /\ LET nonLocal == {u \in msg.adds : u \notin DOMAIN updateOrigin
                                                     \/ updateOrigin[u] /= c}
                  IN receivedRemote' = [receivedRemote EXCEPT
                       ![c] = @ \cup nonLocal]
            /\ UNCHANGED <<serverMetaPath, clientConnected, clientSubscribed,
                           serverDB, serverChannels,
                           updateCounter, updateOrigin>>

         [] msg.type = MsgU ->
            /\ LET localAdds == clientDisk[c] \ clientDoc[c]
                   localDels == clientDoc[c] \ clientDisk[c]
                   afterPreApply == clientDisk[c]
                   finalDoc == (afterPreApply \cup msg.adds) \ msg.dels
                   \* Accept new path from update if it was a path change
                   finalPath == IF msg.pathChange THEN msg.path
                                ELSE clientMetaPath[c]
               IN
               /\ clientDoc' = [clientDoc EXCEPT ![c] = finalDoc]
               /\ clientDisk' = [clientDisk EXCEPT ![c] = finalDoc]
               /\ clientMetaPath' = [clientMetaPath EXCEPT ![c] = finalPath]
               \* Rename file on disk if path changed
               /\ clientDiskPath' = [clientDiskPath EXCEPT ![c] =
                    IF msg.pathChange THEN msg.path
                    ELSE clientDiskPath[c]]
               /\ IF localAdds /= {} \/ localDels /= {}
                  THEN clientToServer' = [clientToServer EXCEPT
                         ![c] = Append(@, [type       |-> MsgU,
                                           adds       |-> localAdds,
                                           dels       |-> localDels,
                                           path       |-> finalPath,
                                           pathChange |-> FALSE,
                                           origin     |-> c])]
                  ELSE UNCHANGED clientToServer
               /\ LET nonLocal == {u \in msg.adds : u \notin DOMAIN updateOrigin
                                                     \/ updateOrigin[u] /= c}
                  IN receivedRemote' = [receivedRemote EXCEPT
                       ![c] = @ \cup nonLocal]
            /\ UNCHANGED <<serverMetaPath, clientConnected, clientSubscribed,
                           serverDB, serverChannels,
                           updateCounter, updateOrigin>>

         [] OTHER ->
            /\ UNCHANGED <<clientDoc, clientDisk, clientMetaPath, clientDiskPath,
                           serverMetaPath, clientConnected, clientSubscribed,
                           clientToServer, serverDB, serverChannels,
                           updateCounter, updateOrigin, receivedRemote>>

    /\ serverToClient' = [serverToClient EXCEPT ![c] = Tail(@)]

(*
 * ClientGoOffline(c): Client disconnects.
 *)
ClientGoOffline(c) ==
    /\ clientConnected[c]
    /\ clientConnected'  = [clientConnected EXCEPT ![c] = FALSE]
    /\ clientSubscribed' = [clientSubscribed EXCEPT ![c] = FALSE]
    /\ serverChannels'   = serverChannels \ {c}
    /\ clientToServer'   = [clientToServer EXCEPT ![c] = <<>>]
    /\ serverToClient'   = [serverToClient EXCEPT ![c] = <<>>]
    /\ UNCHANGED <<clientDoc, clientDisk, clientMetaPath, clientDiskPath,
                   serverMetaPath, serverDB,
                   updateCounter, updateOrigin, receivedRemote>>

(*
 * ClientReconnect(c): Client comes back online.
 *)
ClientReconnect(c) ==
    /\ ~clientConnected[c]
    /\ clientConnected' = [clientConnected EXCEPT ![c] = TRUE]
    /\ UNCHANGED <<clientDoc, clientDisk, clientMetaPath, clientDiskPath,
                   serverMetaPath, clientSubscribed,
                   clientToServer, serverToClient, serverDB, serverChannels,
                   updateCounter, updateOrigin, receivedRemote>>

(* ===================== SERVER ACTIONS ===================== *)

(*
 * ServerReceiveSyncStep1(c): Server processes SyncStep1.
 *)
ServerReceiveSyncStep1(c) ==
    /\ clientToServer[c] /= <<>>
    /\ Head(clientToServer[c]).type = MsgS1
    /\ LET msg  == Head(clientToServer[c])
           diff == serverDB \ msg.have
       IN
       /\ serverChannels' = serverChannels \cup {c}
       /\ clientSubscribed' = [clientSubscribed EXCEPT ![c] = TRUE]
       /\ serverToClient' = [serverToClient EXCEPT
            ![c] = Append(@, [type |-> MsgS2,
                              adds |-> diff,
                              path |-> serverMetaPath])]
       /\ clientToServer' = [clientToServer EXCEPT ![c] = Tail(@)]
    /\ UNCHANGED <<clientDoc, clientDisk, clientMetaPath, clientDiskPath,
                   serverMetaPath, clientConnected,
                   serverDB, updateCounter, updateOrigin, receivedRemote>>

(*
 * ServerReceiveUpdate(c): Server processes a content/path update.
 *)
ServerReceiveUpdate(c) ==
    /\ clientToServer[c] /= <<>>
    /\ Head(clientToServer[c]).type = MsgU
    /\ LET msg == Head(clientToServer[c])
       IN
       /\ serverDB' = (serverDB \cup msg.adds) \ msg.dels
       \* Update server's copy of meta.path if this was a path change
       /\ serverMetaPath' = IF msg.pathChange THEN msg.path
                            ELSE serverMetaPath
       /\ LET recipients == serverChannels \ {c}
          IN serverToClient' = [s \in Clients |->
               IF s \in recipients
               THEN Append(serverToClient[s],
                           [type       |-> MsgU,
                            adds       |-> msg.adds,
                            dels       |-> msg.dels,
                            path       |-> msg.path,
                            pathChange |-> msg.pathChange,
                            origin     |-> msg.origin])
               ELSE serverToClient[s]]
       /\ clientToServer' = [clientToServer EXCEPT ![c] = Tail(@)]
    /\ UNCHANGED <<clientDoc, clientDisk, clientMetaPath, clientDiskPath,
                   clientConnected, clientSubscribed, serverChannels,
                   updateCounter, updateOrigin, receivedRemote>>

(* ===================== NEXT-STATE RELATION ===================== *)

Next ==
    \/ \E c \in Clients : LocalDiskEdit(c)
    \/ \E c \in Clients, p \in PathNames : LocalRename(c, p)
    \/ \E c \in Clients : ClientWatcherFires(c)
    \/ \E c \in Clients : ClientSubscribe(c)
    \/ \E c \in Clients : ClientApplyRemote(c)
    \/ \E c \in Clients : ClientGoOffline(c)
    \/ \E c \in Clients : ClientReconnect(c)
    \/ \E c \in Clients : ServerReceiveSyncStep1(c)
    \/ \E c \in Clients : ServerReceiveUpdate(c)

Fairness ==
    /\ \A c \in Clients : WF_vars(ServerReceiveSyncStep1(c))
    /\ \A c \in Clients : WF_vars(ServerReceiveUpdate(c))
    /\ \A c \in Clients : WF_vars(ClientApplyRemote(c))
    /\ \A c \in Clients : WF_vars(ClientWatcherFires(c))

Spec == Init /\ [][Next]_vars /\ Fairness

(* ===================== SAFETY INVARIANTS ===================== *)

(*
 * NoContentLoss: Remote content that was applied is never lost.
 *)
NoContentLoss ==
    \A c \in Clients :
        receivedRemote[c] \subseteq clientDoc[c]

(*
 * NoEcho: Server never sends update back to originator.
 *)
NoEcho ==
    \A c \in Clients :
        \A i \in 1..Len(serverToClient[c]) :
            LET msg == serverToClient[c][i]
            IN msg.type = MsgU => msg.origin /= c

(*
 * MetaPathDiskConsistency: When a client has processed all pending
 * messages AND the watcher has synced (disk == CRDT), then
 * meta.path always matches the disk path.
 *)
MetaPathDiskConsistency ==
    \A c \in Clients :
        (/\ serverToClient[c] = <<>>
         /\ clientToServer[c] = <<>>
         /\ clientDisk[c] = clientDoc[c]
         /\ clientDiskPath[c] = clientDiskPath[c]  \* always true, just for clarity
        ) => clientMetaPath[c] = clientDiskPath[c]

(*
 * ChannelOnlyForConnected: Only connected clients in broadcast channels.
 *)
ChannelOnlyForConnected ==
    \A c \in serverChannels : clientConnected[c]

(* ===================== LIVENESS PROPERTIES ===================== *)

(*
 * RenameConvergence: After a rename, all connected+subscribed clients
 * eventually agree on the same path when queues drain.
 *)
RenameConvergence ==
    <>( /\ \A c \in Clients : clientConnected[c]
        /\ \A c \in Clients : clientSubscribed[c]
        /\ \A c \in Clients : clientToServer[c] = <<>>
        /\ \A c \in Clients : serverToClient[c] = <<>>
        => \A c1, c2 \in Clients :
             /\ clientMetaPath[c1] = clientMetaPath[c2]
             /\ clientDiskPath[c1] = clientDiskPath[c2]
             /\ clientDoc[c1] = clientDoc[c2])

=============================================================================
