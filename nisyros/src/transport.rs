//! Fixed-membership bootstrap framing for two Nisyros processes.

use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::thread;
use std::time::{Duration, Instant};

const MAGIC: &[u8; 8] = b"NISYROS1";
const GROUP_MAGIC: &[u8; 8] = b"NISYRG01";
const FRAME_MAGIC: &[u8; 8] = b"NISYRF01";
const VERSION: u32 = 1;
const MAX_DEVICES: usize = 32;
const MAX_FAMILIES: usize = 32;
pub const MAX_MESSAGE_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct MessageKey {
    pub group_epoch: u64,
    pub source_rank: u32,
    pub destination_rank: u32,
    pub channel: u64,
    pub sequence: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameKind {
    Offer = 1,
    Ready = 2,
    Data = 3,
    Ack = 4,
    Heartbeat = 5,
    CollectiveContribution = 6,
    CollectiveResult = 7,
}

pub struct Frame {
    pub kind: FrameKind,
    pub key: MessageKey,
    pub bytes: u64,
    pub payload: Vec<u8>,
}

impl Frame {
    pub fn heartbeat() -> Self {
        Self {
            kind: FrameKind::Heartbeat,
            key: MessageKey {
                group_epoch: 0,
                source_rank: 0,
                destination_rank: 0,
                channel: 0,
                sequence: 0,
            },
            bytes: 0,
            payload: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueueAnnouncement {
    pub flags: u32,
    pub count: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceAnnouncement {
    pub backend: u32,
    pub api_version: u32,
    pub vendor_id: u32,
    pub device_id: u32,
    pub device_type: u32,
    pub driver_id: u32,
    pub strict_profile_supported: bool,
    pub name: [u8; 256],
    pub device_uuid: [u8; 16],
    pub driver_uuid: [u8; 16],
    pub driver_name: [u8; 256],
    pub queue_families: Vec<QueueAnnouncement>,
}

#[derive(Debug)]
pub enum JoinError {
    Invalid,
    Timeout,
    PeerLost,
}

pub struct Connection {
    stream: TcpStream,
    failed: bool,
}

impl Connection {
    pub fn into_stream(self) -> TcpStream {
        self.stream
    }
    pub fn exchange_group(
        &mut self,
        epoch: u64,
        members: &[(u64, u32, u32)],
    ) -> Result<(), JoinError> {
        if self.failed || epoch == 0 || members.is_empty() || members.len() > MAX_DEVICES * 2 {
            return Err(JoinError::Invalid);
        }
        let result = (|| {
            self.stream
                .write_all(GROUP_MAGIC)
                .map_err(|_| JoinError::PeerLost)?;
            write_u32(&mut self.stream, VERSION).map_err(|_| JoinError::PeerLost)?;
            write_u64(&mut self.stream, epoch).map_err(|_| JoinError::PeerLost)?;
            write_u32(&mut self.stream, members.len() as u32).map_err(|_| JoinError::PeerLost)?;
            for &(device_epoch, index, backend) in members {
                write_u64(&mut self.stream, device_epoch).map_err(|_| JoinError::PeerLost)?;
                write_u32(&mut self.stream, index).map_err(|_| JoinError::PeerLost)?;
                write_u32(&mut self.stream, backend).map_err(|_| JoinError::PeerLost)?;
            }
            self.stream.flush().map_err(|_| JoinError::PeerLost)?;
            let mut magic = [0; 8];
            self.stream.read_exact(&mut magic).map_err(map_read)?;
            if &magic != GROUP_MAGIC
                || read_u32(&mut self.stream).map_err(map_read)? != VERSION
                || read_u64(&mut self.stream).map_err(map_read)? != epoch
                || read_u32(&mut self.stream).map_err(map_read)? as usize != members.len()
            {
                return Err(JoinError::Invalid);
            }
            for &expected in members {
                let actual = (
                    read_u64(&mut self.stream).map_err(map_read)?,
                    read_u32(&mut self.stream).map_err(map_read)?,
                    read_u32(&mut self.stream).map_err(map_read)?,
                );
                if actual != expected {
                    return Err(JoinError::Invalid);
                }
            }
            Ok(())
        })();
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    pub fn shutdown(self) {
        let _ = self.stream.shutdown(Shutdown::Both);
    }
}

pub fn write_frame(writer: &mut impl Write, frame: &Frame) -> io::Result<()> {
    if frame.bytes > MAX_MESSAGE_BYTES
        || frame.payload.len() as u64 > MAX_MESSAGE_BYTES
        || !valid_payload(frame.kind, frame.bytes, frame.payload.len() as u64)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid frame length",
        ));
    }
    writer.write_all(FRAME_MAGIC)?;
    write_u32(writer, VERSION)?;
    write_u32(writer, frame.kind as u32)?;
    write_u64(writer, frame.key.group_epoch)?;
    write_u32(writer, frame.key.source_rank)?;
    write_u32(writer, frame.key.destination_rank)?;
    write_u64(writer, frame.key.channel)?;
    write_u64(writer, frame.key.sequence)?;
    write_u64(writer, frame.bytes)?;
    write_u32(writer, frame.payload.len() as u32)?;
    writer.write_all(&frame.payload)?;
    writer.flush()
}

pub fn read_frame(reader: &mut impl Read) -> Result<Frame, JoinError> {
    let mut magic = [0; 8];
    reader.read_exact(&mut magic).map_err(map_read)?;
    let version = read_u32(reader).map_err(map_read)?;
    let kind = match read_u32(reader).map_err(map_read)? {
        1 => FrameKind::Offer,
        2 => FrameKind::Ready,
        3 => FrameKind::Data,
        4 => FrameKind::Ack,
        5 => FrameKind::Heartbeat,
        6 => FrameKind::CollectiveContribution,
        7 => FrameKind::CollectiveResult,
        _ => return Err(JoinError::Invalid),
    };
    let key = MessageKey {
        group_epoch: read_u64(reader).map_err(map_read)?,
        source_rank: read_u32(reader).map_err(map_read)?,
        destination_rank: read_u32(reader).map_err(map_read)?,
        channel: read_u64(reader).map_err(map_read)?,
        sequence: read_u64(reader).map_err(map_read)?,
    };
    let bytes = read_u64(reader).map_err(map_read)?;
    let payload_len = read_u32(reader).map_err(map_read)? as u64;
    if &magic != FRAME_MAGIC
        || version != VERSION
        || bytes > MAX_MESSAGE_BYTES
        || payload_len > MAX_MESSAGE_BYTES
        || !valid_payload(kind, bytes, payload_len)
        || (kind == FrameKind::Heartbeat
            && (key.group_epoch != 0
                || key.source_rank != 0
                || key.destination_rank != 0
                || key.channel != 0
                || key.sequence != 0
                || bytes != 0))
    {
        return Err(JoinError::Invalid);
    }
    let mut payload = vec![0; payload_len as usize];
    reader.read_exact(&mut payload).map_err(map_read)?;
    Ok(Frame {
        kind,
        key,
        bytes,
        payload,
    })
}

fn valid_payload(kind: FrameKind, bytes: u64, payload_len: u64) -> bool {
    match kind {
        FrameKind::Data | FrameKind::CollectiveResult => payload_len == bytes,
        FrameKind::CollectiveContribution => bytes
            .checked_add(16)
            .is_some_and(|length| payload_len == length),
        FrameKind::Offer | FrameKind::Ready | FrameKind::Ack | FrameKind::Heartbeat => {
            payload_len == 0
        }
    }
}

pub fn join(
    session_id: u64,
    local_node: u32,
    address: SocketAddr,
    local_devices: &[DeviceAnnouncement],
    timeout: Duration,
) -> Result<(Connection, Vec<DeviceAnnouncement>), JoinError> {
    if session_id == 0 || local_node > 1 || timeout.is_zero() || local_devices.len() > MAX_DEVICES {
        return Err(JoinError::Invalid);
    }
    let started = Instant::now();
    let mut stream = if local_node == 0 {
        let listener = TcpListener::bind(address).map_err(|_| JoinError::PeerLost)?;
        listener
            .set_nonblocking(true)
            .map_err(|_| JoinError::PeerLost)?;
        loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error)
                    if error.kind() == io::ErrorKind::WouldBlock && started.elapsed() < timeout =>
                {
                    thread::sleep(Duration::from_millis(2));
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    return Err(JoinError::Timeout);
                }
                Err(_) => return Err(JoinError::PeerLost),
            }
        }
    } else {
        loop {
            let remaining = timeout.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                return Err(JoinError::Timeout);
            }
            match TcpStream::connect_timeout(&address, remaining.min(Duration::from_millis(100))) {
                Ok(stream) => break stream,
                Err(_) if started.elapsed() < timeout => thread::sleep(Duration::from_millis(2)),
                Err(_) => return Err(JoinError::Timeout),
            }
        }
    };
    let remaining = timeout.saturating_sub(started.elapsed());
    if remaining.is_zero() {
        return Err(JoinError::Timeout);
    }
    stream.set_nodelay(true).map_err(|_| JoinError::PeerLost)?;
    stream
        .set_read_timeout(Some(remaining))
        .map_err(|_| JoinError::PeerLost)?;
    stream
        .set_write_timeout(Some(remaining))
        .map_err(|_| JoinError::PeerLost)?;
    write_announcement(&mut stream, session_id, local_node, local_devices)
        .map_err(|_| JoinError::PeerLost)?;
    let remote = read_announcement(&mut stream, session_id, 1 - local_node)?;
    Ok((
        Connection {
            stream,
            failed: false,
        },
        remote,
    ))
}

fn write_u32(writer: &mut impl Write, value: u32) -> io::Result<()> {
    writer.write_all(&value.to_be_bytes())
}

fn write_u64(writer: &mut impl Write, value: u64) -> io::Result<()> {
    writer.write_all(&value.to_be_bytes())
}

fn read_u32(reader: &mut impl Read) -> io::Result<u32> {
    let mut bytes = [0; 4];
    reader.read_exact(&mut bytes)?;
    Ok(u32::from_be_bytes(bytes))
}

fn read_u64(reader: &mut impl Read) -> io::Result<u64> {
    let mut bytes = [0; 8];
    reader.read_exact(&mut bytes)?;
    Ok(u64::from_be_bytes(bytes))
}

fn write_announcement(
    writer: &mut impl Write,
    session_id: u64,
    node: u32,
    devices: &[DeviceAnnouncement],
) -> io::Result<()> {
    writer.write_all(MAGIC)?;
    write_u32(writer, VERSION)?;
    write_u64(writer, session_id)?;
    write_u32(writer, node)?;
    write_u32(writer, devices.len() as u32)?;
    for device in devices {
        write_u32(writer, device.backend)?;
        write_u32(writer, device.api_version)?;
        write_u32(writer, device.vendor_id)?;
        write_u32(writer, device.device_id)?;
        write_u32(writer, device.device_type)?;
        write_u32(writer, device.driver_id)?;
        write_u32(writer, u32::from(device.strict_profile_supported))?;
        write_u32(writer, device.queue_families.len() as u32)?;
        writer.write_all(&device.name)?;
        writer.write_all(&device.device_uuid)?;
        writer.write_all(&device.driver_uuid)?;
        writer.write_all(&device.driver_name)?;
        for queue in &device.queue_families {
            write_u32(writer, queue.flags)?;
            write_u32(writer, queue.count)?;
        }
    }
    writer.flush()
}

fn read_announcement(
    reader: &mut impl Read,
    session_id: u64,
    peer_node: u32,
) -> Result<Vec<DeviceAnnouncement>, JoinError> {
    let mut magic = [0; 8];
    reader.read_exact(&mut magic).map_err(map_read)?;
    if &magic != MAGIC
        || read_u32(reader).map_err(map_read)? != VERSION
        || read_u64(reader).map_err(map_read)? != session_id
        || read_u32(reader).map_err(map_read)? != peer_node
    {
        return Err(JoinError::Invalid);
    }
    let count = read_u32(reader).map_err(map_read)? as usize;
    if count > MAX_DEVICES {
        return Err(JoinError::Invalid);
    }
    let mut devices = Vec::with_capacity(count);
    for _ in 0..count {
        let backend = read_u32(reader).map_err(map_read)?;
        let api_version = read_u32(reader).map_err(map_read)?;
        let vendor_id = read_u32(reader).map_err(map_read)?;
        let device_id = read_u32(reader).map_err(map_read)?;
        let device_type = read_u32(reader).map_err(map_read)?;
        let driver_id = read_u32(reader).map_err(map_read)?;
        let strict = read_u32(reader).map_err(map_read)?;
        let family_count = read_u32(reader).map_err(map_read)? as usize;
        if backend != 1 || strict > 1 || family_count > MAX_FAMILIES {
            return Err(JoinError::Invalid);
        }
        let mut name = [0; 256];
        let mut device_uuid = [0; 16];
        let mut driver_uuid = [0; 16];
        let mut driver_name = [0; 256];
        reader.read_exact(&mut name).map_err(map_read)?;
        reader.read_exact(&mut device_uuid).map_err(map_read)?;
        reader.read_exact(&mut driver_uuid).map_err(map_read)?;
        reader.read_exact(&mut driver_name).map_err(map_read)?;
        if !name.contains(&0) || !driver_name.contains(&0) {
            return Err(JoinError::Invalid);
        }
        let mut queue_families = Vec::with_capacity(family_count);
        for _ in 0..family_count {
            queue_families.push(QueueAnnouncement {
                flags: read_u32(reader).map_err(map_read)?,
                count: read_u32(reader).map_err(map_read)?,
            });
        }
        devices.push(DeviceAnnouncement {
            backend,
            api_version,
            vendor_id,
            device_id,
            device_type,
            driver_id,
            strict_profile_supported: strict != 0,
            name,
            device_uuid,
            driver_uuid,
            driver_name,
            queue_families,
        });
    }
    Ok(devices)
}

fn map_read(error: io::Error) -> JoinError {
    if error.kind() == io::ErrorKind::TimedOut || error.kind() == io::ErrorKind::WouldBlock {
        JoinError::Timeout
    } else {
        JoinError::PeerLost
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_mismatched_epoch_and_truncated_inventory() {
        let device = DeviceAnnouncement {
            backend: 1,
            api_version: 1,
            vendor_id: 2,
            device_id: 3,
            device_type: 4,
            driver_id: 5,
            strict_profile_supported: false,
            name: [0; 256],
            device_uuid: [1; 16],
            driver_uuid: [2; 16],
            driver_name: [0; 256],
            queue_families: vec![QueueAnnouncement { flags: 2, count: 1 }],
        };
        let mut encoded = Vec::new();
        write_announcement(&mut encoded, 42, 1, std::slice::from_ref(&device)).unwrap();
        assert_eq!(
            read_announcement(&mut encoded.as_slice(), 42, 1).unwrap(),
            vec![device]
        );
        assert!(matches!(
            read_announcement(&mut encoded.as_slice(), 43, 1),
            Err(JoinError::Invalid)
        ));
        assert!(matches!(
            read_announcement(&mut &encoded[..encoded.len() - 1], 42, 1),
            Err(JoinError::PeerLost)
        ));
    }

    #[test]
    fn framed_messages_keep_match_keys_and_reject_bad_lengths() {
        let frame = Frame {
            kind: FrameKind::Data,
            key: MessageKey {
                group_epoch: 9,
                source_rank: 0,
                destination_rank: 2,
                channel: 4,
                sequence: 7,
            },
            bytes: 3,
            payload: vec![1, 2, 3],
        };
        let mut encoded = Vec::new();
        write_frame(&mut encoded, &frame).unwrap();
        let decoded = read_frame(&mut encoded.as_slice()).unwrap();
        assert_eq!(decoded.key, frame.key);
        assert_eq!(decoded.payload, frame.payload);
        assert!(matches!(
            read_frame(&mut &encoded[..encoded.len() - 1]),
            Err(JoinError::PeerLost)
        ));
        encoded[56..60].copy_from_slice(&4_u32.to_be_bytes());
        assert!(matches!(
            read_frame(&mut encoded.as_slice()),
            Err(JoinError::Invalid)
        ));
    }
}
