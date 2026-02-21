use anyhow::{Result, anyhow};
use std::convert::TryFrom;

#[derive(Debug, PartialEq, Clone)]
pub enum MsgType {
    SyncStep1 = 0x00,
    SyncStep2 = 0x01,
    Update = 0x02,
}

impl TryFrom<u8> for MsgType {
    type Error = anyhow::Error;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0x00 => Ok(MsgType::SyncStep1),
            0x01 => Ok(MsgType::SyncStep2),
            0x02 => Ok(MsgType::Update),
            _ => Err(anyhow!("Invalid message type: {}", value)),
        }
    }
}

pub struct Message {
    pub msg_type: MsgType,
    pub doc_id: String,
    pub payload: Vec<u8>,
}

impl Message {
    pub fn new(msg_type: MsgType, doc_id: String, payload: Vec<u8>) -> Self {
        Self {
            msg_type,
            doc_id,
            payload,
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let doc_id_bytes = self.doc_id.as_bytes();
        let doc_id_len = doc_id_bytes.len() as u16;
        let mut buf = Vec::with_capacity(1 + 2 + doc_id_bytes.len() + self.payload.len());

        buf.push(self.msg_type.clone() as u8);
        buf.extend_from_slice(&doc_id_len.to_be_bytes());
        buf.extend_from_slice(doc_id_bytes);
        buf.extend_from_slice(&self.payload);

        buf
    }

    pub fn decode(buf: &[u8]) -> Result<Self> {
        if buf.len() < 3 {
            return Err(anyhow!("Message too short"));
        }

        let msg_type = MsgType::try_from(buf[0])?;

        let mut len_bytes = [0u8; 2];
        len_bytes.copy_from_slice(&buf[1..3]);
        let doc_id_len = u16::from_be_bytes(len_bytes) as usize;

        if buf.len() < 3 + doc_id_len {
            return Err(anyhow!("Message too short for doc_id length"));
        }

        let doc_id = String::from_utf8(buf[3..3 + doc_id_len].to_vec())?;

        let payload = buf[3 + doc_id_len..].to_vec();

        Ok(Self {
            msg_type,
            doc_id,
            payload,
        })
    }
}
