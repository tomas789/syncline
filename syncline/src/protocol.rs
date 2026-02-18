pub const MSG_SYNC_STEP_1: u8 = 0;
pub const MSG_SYNC_STEP_2: u8 = 1;
pub const MSG_UPDATE: u8 = 2;

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
