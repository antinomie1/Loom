// SPDX-License-Identifier: BSD-2-Clause

use thiserror::Error;

const MAGIC: u32 = u32::from_le_bytes(*b"LOOM");
const VERSION: u16 = 1;
const HEADER_LEN: usize = 28;
pub const MAX_PACKET_LEN: usize = 64 * 1024;
pub const MAX_PAYLOAD_LEN: usize = MAX_PACKET_LEN - HEADER_LEN;
const FLAG_MORE: u16 = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u16)]
pub enum MessageKind {
    Request = 1,
    Response = 2,
}

impl TryFrom<u16> for MessageKind {
    type Error = ProtocolError;

    fn try_from(value: u16) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Request),
            2 => Ok(Self::Response),
            _ => Err(ProtocolError::InvalidMessageKind(value)),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u16)]
pub enum Operation {
    Start = 1,
    Stop = 2,
    Restart = 3,
    ReloadService = 4,
    Status = 5,
    List = 6,
    IsActive = 7,
    IsEnabled = 8,
    Dependencies = 9,
    Enable = 10,
    Disable = 11,
    Reload = 12,
    Apply = 13,
    ResetFailed = 14,
    Timings = 15,
    CriticalPath = 16,
    Reboot = 17,
    Poweroff = 18,
    ApplyDryRun = 19,
    StopForce = 20,
}

impl TryFrom<u16> for Operation {
    type Error = ProtocolError;

    fn try_from(value: u16) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Start),
            2 => Ok(Self::Stop),
            3 => Ok(Self::Restart),
            4 => Ok(Self::ReloadService),
            5 => Ok(Self::Status),
            6 => Ok(Self::List),
            7 => Ok(Self::IsActive),
            8 => Ok(Self::IsEnabled),
            9 => Ok(Self::Dependencies),
            10 => Ok(Self::Enable),
            11 => Ok(Self::Disable),
            12 => Ok(Self::Reload),
            13 => Ok(Self::Apply),
            14 => Ok(Self::ResetFailed),
            15 => Ok(Self::Timings),
            16 => Ok(Self::CriticalPath),
            17 => Ok(Self::Reboot),
            18 => Ok(Self::Poweroff),
            19 => Ok(Self::ApplyDryRun),
            20 => Ok(Self::StopForce),
            _ => Err(ProtocolError::InvalidOperation(value)),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u16)]
pub enum StatusCode {
    Ok = 0,
    ServiceFailure = 1,
    InvalidRequest = 2,
    PermissionDenied = 3,
    ManagerUnavailable = 4,
    Timeout = 5,
    NotFound = 6,
    Conflict = 7,
    Internal = 8,
}

impl TryFrom<u16> for StatusCode {
    type Error = ProtocolError;

    fn try_from(value: u16) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Ok),
            1 => Ok(Self::ServiceFailure),
            2 => Ok(Self::InvalidRequest),
            3 => Ok(Self::PermissionDenied),
            4 => Ok(Self::ManagerUnavailable),
            5 => Ok(Self::Timeout),
            6 => Ok(Self::NotFound),
            7 => Ok(Self::Conflict),
            8 => Ok(Self::Internal),
            _ => Err(ProtocolError::InvalidStatus(value)),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Packet {
    pub kind: MessageKind,
    pub request_id: u64,
    pub operation: Operation,
    pub status: StatusCode,
    pub more: bool,
    pub payload: Vec<u8>,
}

impl Packet {
    /// Encodes one bounded protocol packet in the stable little-endian format.
    ///
    /// # Errors
    ///
    /// Returns [`ProtocolError::PayloadTooLarge`] when the packet cannot fit in
    /// one `SOCK_SEQPACKET` message.
    pub fn encode(&self) -> Result<Vec<u8>, ProtocolError> {
        if self.payload.len() > MAX_PAYLOAD_LEN {
            return Err(ProtocolError::PayloadTooLarge(self.payload.len()));
        }
        let payload_len = u32::try_from(self.payload.len())
            .map_err(|_| ProtocolError::PayloadTooLarge(self.payload.len()))?;
        let mut encoded = Vec::with_capacity(HEADER_LEN + self.payload.len());
        encoded.extend_from_slice(&MAGIC.to_le_bytes());
        encoded.extend_from_slice(&VERSION.to_le_bytes());
        encoded.extend_from_slice(&(self.kind as u16).to_le_bytes());
        encoded.extend_from_slice(&self.request_id.to_le_bytes());
        encoded.extend_from_slice(&(self.operation as u16).to_le_bytes());
        encoded.extend_from_slice(&(self.status as u16).to_le_bytes());
        encoded.extend_from_slice(&(u16::from(self.more) * FLAG_MORE).to_le_bytes());
        encoded.extend_from_slice(&0_u16.to_le_bytes());
        encoded.extend_from_slice(&payload_len.to_le_bytes());
        encoded.extend_from_slice(&self.payload);
        Ok(encoded)
    }

    /// Decodes and validates exactly one protocol packet.
    ///
    /// # Errors
    ///
    /// Returns an error for truncation, excess bytes, unknown enum values,
    /// unsupported versions, reserved bits, or packets over the hard limit.
    pub fn decode(encoded: &[u8]) -> Result<Self, ProtocolError> {
        if encoded.len() > MAX_PACKET_LEN {
            return Err(ProtocolError::PacketTooLarge(encoded.len()));
        }
        if encoded.len() < HEADER_LEN {
            return Err(ProtocolError::Truncated {
                expected: HEADER_LEN,
                actual: encoded.len(),
            });
        }
        let magic = read_u32(encoded, 0);
        if magic != MAGIC {
            return Err(ProtocolError::InvalidMagic(magic));
        }
        let version = read_u16(encoded, 4);
        if version != VERSION {
            return Err(ProtocolError::UnsupportedVersion(version));
        }
        let flags = read_u16(encoded, 20);
        if flags & !FLAG_MORE != 0 || read_u16(encoded, 22) != 0 {
            return Err(ProtocolError::ReservedBits);
        }
        let payload_len = usize::try_from(read_u32(encoded, 24))
            .map_err(|_| ProtocolError::PacketTooLarge(encoded.len()))?;
        let expected = HEADER_LEN
            .checked_add(payload_len)
            .ok_or(ProtocolError::PacketTooLarge(encoded.len()))?;
        if encoded.len() != expected {
            return Err(ProtocolError::LengthMismatch {
                declared: payload_len,
                actual: encoded.len() - HEADER_LEN,
            });
        }

        Ok(Self {
            kind: MessageKind::try_from(read_u16(encoded, 6))?,
            request_id: read_u64(encoded, 8),
            operation: Operation::try_from(read_u16(encoded, 16))?,
            status: StatusCode::try_from(read_u16(encoded, 18))?,
            more: flags & FLAG_MORE != 0,
            payload: encoded[HEADER_LEN..].to_vec(),
        })
    }
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum ProtocolError {
    #[error("packet is truncated: expected at least {expected} bytes, got {actual}")]
    Truncated { expected: usize, actual: usize },
    #[error("invalid protocol magic 0x{0:08x}")]
    InvalidMagic(u32),
    #[error("unsupported protocol version {0}")]
    UnsupportedVersion(u16),
    #[error("invalid message kind {0}")]
    InvalidMessageKind(u16),
    #[error("invalid operation {0}")]
    InvalidOperation(u16),
    #[error("invalid status code {0}")]
    InvalidStatus(u16),
    #[error("reserved protocol bits are non-zero")]
    ReservedBits,
    #[error("payload is too large: {0} bytes")]
    PayloadTooLarge(usize),
    #[error("packet is too large: {0} bytes")]
    PacketTooLarge(usize),
    #[error("payload length mismatch: declared {declared}, received {actual}")]
    LengthMismatch { declared: usize, actual: usize },
}

fn read_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
        bytes[offset + 4],
        bytes[offset + 5],
        bytes[offset + 6],
        bytes[offset + 7],
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet() -> Packet {
        Packet {
            kind: MessageKind::Response,
            request_id: 42,
            operation: Operation::Status,
            status: StatusCode::Ok,
            more: true,
            payload: b"active".to_vec(),
        }
    }

    #[test]
    fn round_trips_packet() {
        let packet = packet();
        assert_eq!(Packet::decode(&packet.encode().unwrap()).unwrap(), packet);
    }

    #[test]
    fn rejects_truncated_excess_and_reserved_data() {
        let encoded = packet().encode().unwrap();
        assert!(matches!(
            Packet::decode(&encoded[..10]),
            Err(ProtocolError::Truncated { .. })
        ));

        let mut excess = encoded.clone();
        excess.push(0);
        assert!(matches!(
            Packet::decode(&excess),
            Err(ProtocolError::LengthMismatch { .. })
        ));

        let mut reserved = encoded;
        reserved[22] = 1;
        assert_eq!(Packet::decode(&reserved), Err(ProtocolError::ReservedBits));
    }

    #[test]
    fn enforces_packet_bound() {
        let mut packet = packet();
        packet.payload = vec![0; MAX_PAYLOAD_LEN + 1];
        assert!(matches!(
            packet.encode(),
            Err(ProtocolError::PayloadTooLarge(_))
        ));
    }

    #[test]
    fn operation_numbers_are_stable() {
        assert_eq!(Operation::Start as u16, 1);
        assert_eq!(Operation::Apply as u16, 13);
        assert_eq!(Operation::Poweroff as u16, 18);
    }
}
