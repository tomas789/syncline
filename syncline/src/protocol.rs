pub const MSG_SYNC_STEP_1: u8 = 0;
pub const MSG_SYNC_STEP_2: u8 = 1;
pub const MSG_UPDATE: u8 = 2;
pub const MSG_BLOB_UPDATE: u8 = 4;
pub const MSG_BLOB_REQUEST: u8 = 5;
/// Periodic reconciliation: client sends its state vector; server responds
/// with MSG_SYNC_STEP_2 containing any missing updates.  Unlike MSG_SYNC_STEP_1,
/// this does NOT subscribe the client to the broadcast channel (it's already
/// subscribed from the initial SyncStep1).
pub const MSG_RESYNC: u8 = 6;
/// Convergence checksum: client sends SHA256 of text content for a doc.
/// If the server's content disagrees, it responds with a full SyncStep2.
pub const MSG_CHECKSUM: u8 = 7;

/// Maximum blob size in bytes (50 MB).
pub const MAX_BLOB_SIZE: usize = 50 * 1024 * 1024;

pub fn encode_message(msg_type: u8, doc_id: &str, payload: &[u8]) -> Vec<u8> {
    let doc_id_bytes = doc_id.as_bytes();
    let mut msg = Vec::with_capacity(1 + 2 + doc_id_bytes.len() + payload.len());
    msg.push(msg_type);
    msg.extend_from_slice(&(doc_id_bytes.len() as u16).to_be_bytes());
    msg.extend_from_slice(doc_id_bytes);
    msg.extend_from_slice(payload);
    msg
}

pub fn decode_message(data: &[u8]) -> Option<(u8, &str, &[u8])> {
    if data.len() < 3 {
        return None;
    }
    let msg_type = data[0];
    let doc_id_len = u16::from_be_bytes([data[1], data[2]]) as usize;
    if data.len() < 3 + doc_id_len {
        return None;
    }
    let doc_id = std::str::from_utf8(&data[3..3 + doc_id_len]).ok()?;
    let payload = &data[3 + doc_id_len..];
    Some((msg_type, doc_id, payload))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_decode() {
        let payload = vec![0x01, 0x02, 0x03];
        let encoded = encode_message(MSG_SYNC_STEP_1, "test_doc", &payload);

        // 1 byte msg_type + 2 bytes len + 8 bytes doc_id + 3 bytes payload = 14 bytes
        assert_eq!(encoded.len(), 14);

        let decoded = decode_message(&encoded);
        assert!(decoded.is_some());

        let (msg_type, doc_id, decoded_payload) = decoded.unwrap();
        assert_eq!(msg_type, MSG_SYNC_STEP_1);
        assert_eq!(doc_id, "test_doc");
        assert_eq!(decoded_payload, payload.as_slice());
    }

    #[test]
    fn test_decode_invalid_length() {
        assert!(decode_message(&[0, 0]).is_none());
    }

    #[test]
    fn test_decode_invalid_doc_id_length() {
        // payload size too short based on doc_id_len
        let encoded = vec![MSG_UPDATE, 0, 10, b'a']; // doc_id length 10, but only 1 byte available
        assert!(decode_message(&encoded).is_none());
    }

    #[test]
    fn test_decode_invalid_utf8() {
        // valid length, but invalid UTF-8 for doc_id
        let mut encoded = vec![MSG_UPDATE, 0, 1];
        encoded.push(0xff); // invalid UTF-8 byte
        assert!(decode_message(&encoded).is_none());
    }

    #[test]
    fn test_encode_decode_blob_update() {
        let blob_data = vec![0x89, 0x50, 0x4e, 0x47]; // PNG header bytes
        let encoded = encode_message(MSG_BLOB_UPDATE, "image.png", &blob_data);

        let (msg_type, doc_id, payload) = decode_message(&encoded).unwrap();
        assert_eq!(msg_type, MSG_BLOB_UPDATE);
        assert_eq!(doc_id, "image.png");
        assert_eq!(payload, blob_data.as_slice());
    }

    #[test]
    fn test_encode_decode_blob_request() {
        // 32-byte SHA256 hash as payload
        let hash_bytes = [0xABu8; 32];
        let encoded = encode_message(MSG_BLOB_REQUEST, "image.png", &hash_bytes);

        let (msg_type, doc_id, payload) = decode_message(&encoded).unwrap();
        assert_eq!(msg_type, MSG_BLOB_REQUEST);
        assert_eq!(doc_id, "image.png");
        assert_eq!(payload, &hash_bytes);
    }
}
