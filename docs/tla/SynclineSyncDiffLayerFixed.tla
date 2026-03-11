--------------------- MODULE SynclineSyncDiffLayerFixed ---------------------
(*
 * Phase 2 FIXED: Diff Layer with atomic write-after-apply.
 *
 * This is identical to SynclineSyncDiffLayer except ClientApplyRemote
 * ATOMICALLY writes to disk after applying the remote update.
 * This models the folder client's behavior (pre-apply + immediate write)
 * where disk is always kept in sync with the CRDT.
 *
 * EXPECTED RESULT: NoContentLoss should PASS — the fix prevents
 * stale diffs from generating spurious deletes.
 *)
EXTENDS Naturals, Sequences, FiniteSets, TLC

CONSTANTS Clients, DocIds, MaxUpdates, MaxQueueLen

VARIABLES
    clientDoc, clientDisk, clientIgnoring,
    clientConnected, clientSubscribed,
    clientToServer, serverToClient,
    serverDB, serverChannels, serverIndex,
    updateCounter, updateOrigin, receivedRemote

vars == <<clientDoc, clientDisk, clientIgnoring,
          clientConnected, clientSubscribed,
          clientToServer, serverToClient,
          serverDB, serverChannels, serverIndex,
          updateCounter, updateOrigin, receivedRemote>>

StateConstraint ==
    /\ \A c \in Clients : Len(clientToServer[c]) <= MaxQueueLen
    /\ \A c \in Clients : Len(serverToClient[c]) <= MaxQueueLen
    /\ updateCounter <= MaxUpdates

MsgS1 == "S1"
MsgS2 == "S2"
MsgU  == "U"

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

ClientWatcherFires(c, d) ==
    /\ clientConnected[c]
    /\ d \notin clientIgnoring[c]
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
 * ClientApplyRemote(c): THE FIX — atomically applies remote update
 * AND writes to disk in the same step. This prevents any interleaving
 * where the watcher fires between CRDT update and disk write.
 *
 * This matches the folder client's behavior where the event loop
 * processes the remote update and writes to disk synchronously.
 *)
ClientApplyRemote(c) ==
    /\ clientConnected[c]
    /\ serverToClient[c] /= <<>>
    /\ LET msg == Head(serverToClient[c])
       IN
       CASE msg.type = MsgS2 ->
            /\ clientDoc' = [clientDoc EXCEPT
                 ![c][msg.docId] = @ \cup msg.adds]
            \* FIX: atomically write to disk
            /\ clientDisk' = [clientDisk EXCEPT
                 ![c][msg.docId] = clientDoc'[c][msg.docId]]
            /\ clientIgnoring' = [clientIgnoring EXCEPT
                 ![c] = @ \cup {msg.docId}]
            /\ LET nonLocal == {u \in msg.adds : u \notin DOMAIN updateOrigin
                                                  \/ updateOrigin[u] /= c}
               IN receivedRemote' = [receivedRemote EXCEPT
                    ![c][msg.docId] = @ \cup nonLocal]

         [] msg.type = MsgU ->
            /\ clientDoc' = [clientDoc EXCEPT
                 ![c][msg.docId] = (@ \cup msg.adds) \ msg.dels]
            \* FIX: atomically write to disk
            /\ clientDisk' = [clientDisk EXCEPT
                 ![c][msg.docId] = clientDoc'[c][msg.docId]]
            /\ clientIgnoring' = [clientIgnoring EXCEPT
                 ![c] = @ \cup {msg.docId}]
            /\ LET nonLocal == {u \in msg.adds : u \notin DOMAIN updateOrigin
                                                  \/ updateOrigin[u] /= c}
               IN receivedRemote' = [receivedRemote EXCEPT
                    ![c][msg.docId] = @ \cup nonLocal]

         [] OTHER ->
            /\ UNCHANGED <<clientDoc, clientDisk, clientIgnoring>>
            /\ UNCHANGED receivedRemote

    /\ serverToClient' = [serverToClient EXCEPT ![c] = Tail(@)]
    /\ UNCHANGED <<clientConnected, clientSubscribed,
                   clientToServer, serverDB, serverChannels, serverIndex,
                   updateCounter, updateOrigin>>

ClientIgnoreExpires(c, d) ==
    /\ d \in clientIgnoring[c]
    /\ clientIgnoring' = [clientIgnoring EXCEPT ![c] = @ \ {d}]
    /\ UNCHANGED <<clientDoc, clientDisk, clientConnected, clientSubscribed,
                   clientToServer, serverToClient,
                   serverDB, serverChannels, serverIndex,
                   updateCounter, updateOrigin, receivedRemote>>

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

ClientReconnect(c) ==
    /\ ~clientConnected[c]
    /\ clientConnected' = [clientConnected EXCEPT ![c] = TRUE]
    /\ UNCHANGED <<clientDoc, clientDisk, clientIgnoring, clientSubscribed,
                   clientToServer, serverToClient, serverDB, serverChannels,
                   serverIndex, updateCounter, updateOrigin, receivedRemote>>

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
                           [type   |-> MsgU, docId |-> d,
                            adds   |-> msg.adds, dels |-> msg.dels,
                            origin |-> msg.origin])
               ELSE serverToClient[s]]
       /\ clientToServer' = [clientToServer EXCEPT ![c] = Tail(@)]
    /\ UNCHANGED <<clientDoc, clientDisk, clientIgnoring, clientConnected,
                   clientSubscribed, serverChannels,
                   updateCounter, updateOrigin, receivedRemote>>

(* ===================== SPECIFICATION ===================== *)

Next ==
    \/ \E c \in Clients, d \in DocIds : LocalDiskEdit(c, d)
    \/ \E c \in Clients, d \in DocIds : ClientWatcherFires(c, d)
    \/ \E c \in Clients, d \in DocIds : ClientSubscribe(c, d)
    \/ \E c \in Clients : ClientApplyRemote(c)
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

(* ===================== PROPERTIES ===================== *)

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

=============================================================================
