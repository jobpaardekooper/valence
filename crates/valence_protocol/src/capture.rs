use std::io::{self, Read, Seek, SeekFrom, Write};

use anyhow::{bail, ensure, Context};
use byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt};

use crate::{CompressionThreshold, PacketSide, PacketState, PROTOCOL_VERSION};

const MAGIC: [u8; 8] = *b"VPCAP001";
const VERSION: u16 = 1;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct CaptureHeader {
    pub protocol_version: i32,
    pub connection_started_unix_ms: i64,
    pub final_compression_threshold: CompressionThreshold,
    pub online_mode: bool,
}

impl CaptureHeader {
    pub fn new(connection_started_unix_ms: i64) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            connection_started_unix_ms,
            final_compression_threshold: CompressionThreshold::DEFAULT,
            online_mode: false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaptureRecord {
    pub sequence: u64,
    pub side: PacketSide,
    pub state: PacketState,
    pub compression_threshold: CompressionThreshold,
    pub packet_id: i32,
    pub raw_bytes: Vec<u8>,
}

pub struct CaptureWriter<W> {
    writer: W,
    header: CaptureHeader,
}

impl<W: Write + Seek> CaptureWriter<W> {
    pub fn new(mut writer: W, header: CaptureHeader) -> anyhow::Result<Self> {
        write_header(&mut writer, header)?;

        Ok(Self { writer, header })
    }

    pub fn write_record(&mut self, record: &CaptureRecord) -> anyhow::Result<()> {
        let raw_len =
            u32::try_from(record.raw_bytes.len()).context("capture packet is too large")?;

        self.writer.write_u64::<LittleEndian>(record.sequence)?;
        self.writer.write_u8(side_to_u8(record.side))?;
        self.writer.write_u8(state_to_u8(record.state))?;
        self.writer
            .write_i32::<LittleEndian>(record.compression_threshold.0)?;
        self.writer.write_i32::<LittleEndian>(record.packet_id)?;
        self.writer.write_u32::<LittleEndian>(raw_len)?;
        self.writer.write_all(&record.raw_bytes)?;

        Ok(())
    }

    pub fn finish(
        mut self,
        final_compression_threshold: CompressionThreshold,
        online_mode: bool,
    ) -> anyhow::Result<W> {
        self.header.final_compression_threshold = final_compression_threshold;
        self.header.online_mode = online_mode;

        let end = self.writer.stream_position()?;
        self.writer.seek(SeekFrom::Start(0))?;
        write_header(&mut self.writer, self.header)?;
        self.writer.seek(SeekFrom::Start(end))?;

        Ok(self.writer)
    }
}

pub struct CaptureReader<R> {
    reader: R,
    header: CaptureHeader,
}

impl<R: Read> CaptureReader<R> {
    pub fn new(mut reader: R) -> anyhow::Result<Self> {
        let header = read_header(&mut reader)?;

        Ok(Self { reader, header })
    }

    pub fn header(&self) -> CaptureHeader {
        self.header
    }

    pub fn read_record(&mut self) -> anyhow::Result<Option<CaptureRecord>> {
        let sequence = match self.reader.read_u64::<LittleEndian>() {
            Ok(sequence) => sequence,
            Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(err) => return Err(err.into()),
        };

        let side = u8_to_side(self.reader.read_u8()?)?;
        let state = u8_to_state(self.reader.read_u8()?)?;
        let compression_threshold = CompressionThreshold(self.reader.read_i32::<LittleEndian>()?);
        let packet_id = self.reader.read_i32::<LittleEndian>()?;
        let raw_len = self.reader.read_u32::<LittleEndian>()?;
        let mut raw_bytes = vec![0; raw_len as usize];
        self.reader.read_exact(&mut raw_bytes)?;

        Ok(Some(CaptureRecord {
            sequence,
            side,
            state,
            compression_threshold,
            packet_id,
            raw_bytes,
        }))
    }
}

fn write_header<W: Write>(writer: &mut W, header: CaptureHeader) -> anyhow::Result<()> {
    writer.write_all(&MAGIC)?;
    writer.write_u16::<LittleEndian>(VERSION)?;
    writer.write_u8(u8::from(header.online_mode))?;
    writer.write_u8(0)?;
    writer.write_i32::<LittleEndian>(header.protocol_version)?;
    writer.write_i64::<LittleEndian>(header.connection_started_unix_ms)?;
    writer.write_i32::<LittleEndian>(header.final_compression_threshold.0)?;

    Ok(())
}

fn read_header<R: Read>(reader: &mut R) -> anyhow::Result<CaptureHeader> {
    let mut magic = [0; MAGIC.len()];
    reader.read_exact(&mut magic)?;
    ensure!(magic == MAGIC, "invalid capture magic");

    let version = reader.read_u16::<LittleEndian>()?;
    ensure!(version == VERSION, "unsupported capture version {version}");

    let online_mode = match reader.read_u8()? {
        0 => false,
        1 => true,
        flag => bail!("invalid online mode flag {flag}"),
    };

    let _reserved = reader.read_u8()?;
    let protocol_version = reader.read_i32::<LittleEndian>()?;
    let connection_started_unix_ms = reader.read_i64::<LittleEndian>()?;
    let final_compression_threshold = CompressionThreshold(reader.read_i32::<LittleEndian>()?);

    Ok(CaptureHeader {
        protocol_version,
        connection_started_unix_ms,
        final_compression_threshold,
        online_mode,
    })
}

fn side_to_u8(side: PacketSide) -> u8 {
    match side {
        PacketSide::Clientbound => 0,
        PacketSide::Serverbound => 1,
    }
}

fn u8_to_side(value: u8) -> anyhow::Result<PacketSide> {
    match value {
        0 => Ok(PacketSide::Clientbound),
        1 => Ok(PacketSide::Serverbound),
        _ => bail!("invalid packet side {value}"),
    }
}

fn state_to_u8(state: PacketState) -> u8 {
    match state {
        PacketState::Handshake => 0,
        PacketState::Status => 1,
        PacketState::Login => 2,
        PacketState::Configuration => 3,
        PacketState::Play => 4,
    }
}

fn u8_to_state(value: u8) -> anyhow::Result<PacketState> {
    match value {
        0 => Ok(PacketState::Handshake),
        1 => Ok(PacketState::Status),
        2 => Ok(PacketState::Login),
        3 => Ok(PacketState::Configuration),
        4 => Ok(PacketState::Play),
        _ => bail!("invalid packet state {value}"),
    }
}
