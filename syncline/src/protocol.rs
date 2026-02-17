pub const MSG_SYNC_STEP_1: u8 = 0; // Client sends SV -> Server replies with Update
pub const MSG_SYNC_STEP_2: u8 = 1; // Server sends Update to Client
pub const MSG_UPDATE: u8 = 2; // Client sends Update -> Server saves & broadcasts
