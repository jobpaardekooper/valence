use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::Context;
use time::OffsetDateTime;
use valence_protocol::capture::{CaptureHeader, CaptureRecord, CaptureWriter};
use valence_protocol::{CompressionThreshold, PacketSide, PacketState};

pub(crate) struct ConnectionCapture {
    client_port: u16,
    output_dir: PathBuf,
    temp_path: PathBuf,
    started_at: OffsetDateTime,
    next_sequence: u64,
    online_mode: bool,
    writer: CaptureWriter<BufWriter<File>>,
}

impl ConnectionCapture {
    pub(crate) fn create(output_dir: PathBuf, client_addr: SocketAddr) -> anyhow::Result<Self> {
        fs::create_dir_all(&output_dir).with_context(|| {
            format!(
                "failed to create capture directory {}",
                output_dir.display()
            )
        })?;

        let started_at = local_time();
        let started_ms = i64::try_from(started_at.unix_timestamp_nanos() / 1_000_000)
            .context("connection start timestamp is out of range")?;
        let temp_path = output_dir.join(format!(
            "{}-{:05}.tmp",
            timestamp_prefix(started_at),
            client_addr.port()
        ));

        let file = File::create(&temp_path)
            .with_context(|| format!("failed to create capture file {}", temp_path.display()))?;
        let writer = CaptureWriter::new(BufWriter::new(file), CaptureHeader::new(started_ms))?;

        Ok(Self {
            client_port: client_addr.port(),
            output_dir,
            temp_path,
            started_at,
            next_sequence: 0,
            online_mode: false,
            writer,
        })
    }

    pub(crate) fn write_packet(
        &mut self,
        side: PacketSide,
        state: PacketState,
        compression_threshold: CompressionThreshold,
        packet_id: i32,
        raw_bytes: &[u8],
    ) -> anyhow::Result<()> {
        self.writer.write_record(&CaptureRecord {
            sequence: self.next_sequence,
            side,
            state,
            compression_threshold,
            packet_id,
            raw_bytes: raw_bytes.to_vec(),
        })?;
        self.next_sequence += 1;
        Ok(())
    }

    pub(crate) fn mark_online_mode(&mut self) {
        self.online_mode = true;
    }

    pub(crate) fn finish(self, final_threshold: CompressionThreshold) -> anyhow::Result<PathBuf> {
        let mut writer = self.writer.finish(final_threshold, self.online_mode)?;
        writer.flush()?;
        drop(writer);

        let final_path = self.output_dir.join(format!(
            "{}-{:05}-{}-{}.vpcap",
            timestamp_prefix(self.started_at),
            self.client_port,
            threshold_label(final_threshold),
            if self.online_mode {
                "online"
            } else {
                "offline"
            }
        ));

        fs::rename(&self.temp_path, &final_path).with_context(|| {
            format!(
                "failed to move capture {} to {}",
                self.temp_path.display(),
                final_path.display()
            )
        })?;

        Ok(final_path)
    }
}

fn local_time() -> OffsetDateTime {
    match OffsetDateTime::now_local() {
        Ok(time) => time,
        Err(_) => OffsetDateTime::now_utc(),
    }
}

fn timestamp_prefix(time: OffsetDateTime) -> String {
    format!(
        "{:04}{:02}{:02}-{:02}{:02}{:02}-{:03}",
        time.year(),
        u8::from(time.month()),
        time.day(),
        time.hour(),
        time.minute(),
        time.second(),
        time.millisecond(),
    )
}

fn threshold_label(threshold: CompressionThreshold) -> String {
    if threshold.0 < 0 {
        "none".to_owned()
    } else {
        threshold.0.to_string()
    }
}
