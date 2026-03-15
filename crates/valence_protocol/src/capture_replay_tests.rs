use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};

use crate::capture::CaptureReader;
use crate::decode::PacketDecoder;
use crate::PacketSide;

include!(concat!(env!("OUT_DIR"), "/capture_replay_dispatch.rs"));

#[test]
fn replay_capture_fixtures() -> anyhow::Result<()> {
    let capture_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("captures");
    let mut capture_files = capture_files(&capture_dir)?;

    if capture_files.is_empty() {
        println!(
            "No capture fixtures found in {}. Skipping.",
            capture_dir.display()
        );
        return Ok(());
    }

    capture_files.sort();

    let mut overall = ReplaySummary::default();
    let mut failures = Vec::new();

    for path in capture_files {
        let file_summary = replay_capture_file(&path, &mut failures)?;
        overall.merge(&file_summary);

        println!(
            "capture={} total={} replayed={} failed={} unique_defined={} coverage={:.2}%",
            path.display(),
            file_summary.total_records,
            file_summary.replayed_records,
            file_summary.failed_records,
            file_summary.seen_defined_packets.len(),
            coverage_pct(file_summary.seen_defined_packets.len(), TOTAL_KNOWN_PACKETS),
        );
    }

    println!(
        "overall total={} replayed={} failed={} unique_defined={} coverage={:.2}%",
        overall.total_records,
        overall.replayed_records,
        overall.failed_records,
        overall.seen_defined_packets.len(),
        coverage_pct(overall.seen_defined_packets.len(), TOTAL_KNOWN_PACKETS),
    );

    if !failures.is_empty() {
        for failure in &failures {
            println!("{failure}");
        }

        anyhow::bail!("capture replay found {} failures", failures.len());
    }

    Ok(())
}

fn replay_capture_file(path: &Path, failures: &mut Vec<String>) -> anyhow::Result<ReplaySummary> {
    let file = File::open(path)?;
    let mut reader = CaptureReader::new(BufReader::new(file))?;
    let header = reader.header();

    let mut summary = ReplaySummary::default();
    let mut original_by_side = side_map();
    let mut replayed_by_side = side_map();

    while let Some(record) = reader.read_record()? {
        summary.total_records += 1;
        original_by_side
            .get_mut(&record.side)
            .unwrap()
            .extend_from_slice(&record.raw_bytes);

        if is_known_packet(record.side, record.state, record.packet_id) {
            summary
                .seen_defined_packets
                .insert((record.side, record.state, record.packet_id));
        }

        match replay_record(&record) {
            Ok((_packet_name, bytes)) => {
                summary.replayed_records += 1;
                replayed_by_side
                    .get_mut(&record.side)
                    .unwrap()
                    .extend_from_slice(&bytes);
            }
            Err(err) => {
                summary.failed_records += 1;
                replayed_by_side
                    .get_mut(&record.side)
                    .unwrap()
                    .extend_from_slice(&record.raw_bytes);

                failures.push(format!(
                    "{} seq={} side={:?} state={:?} id=0x{:02X}: {err:#}",
                    path.display(),
                    record.sequence,
                    record.side,
                    record.state,
                    record.packet_id,
                ));
            }
        }
    }

    for side in [PacketSide::Serverbound, PacketSide::Clientbound] {
        let original = original_by_side.get(&side).unwrap();
        let replayed = replayed_by_side.get(&side).unwrap();

        if original != replayed {
            summary.failed_records += 1;
            failures.push(format!(
                "{} side={side:?}: reconstructed stream mismatch (original={} bytes, replayed={} bytes); final_threshold={}, online_mode={}",
                path.display(),
                original.len(),
                replayed.len(),
                header.final_compression_threshold.0,
                header.online_mode,
            ));
        }
    }

    Ok(summary)
}

fn replay_record(
    record: &crate::capture::CaptureRecord,
) -> anyhow::Result<(&'static str, Vec<u8>)> {
    let mut decoder = PacketDecoder::new();
    decoder.set_compression(record.compression_threshold);
    decoder.queue_slice(&record.raw_bytes);

    let packet = decoder
        .try_next_packet_with_raw()?
        .ok_or_else(|| crate::anyhow::anyhow!("decoder did not produce a packet"))?;

    crate::anyhow::ensure!(
        packet.raw.as_ref() == record.raw_bytes.as_slice(),
        "decoder raw bytes differed from capture input"
    );
    crate::anyhow::ensure!(
        packet.frame.id == record.packet_id,
        "packet ID mismatch: decoded 0x{:02X}, expected 0x{:02X}",
        packet.frame.id,
        record.packet_id,
    );
    crate::anyhow::ensure!(
        decoder.try_next_packet()?.is_none(),
        "decoder had trailing packet data after a single capture record"
    );

    let mut out = Vec::new();
    let packet_name = decode_and_reencode_known_packet(
        record.side,
        record.state,
        record.compression_threshold,
        &packet.frame,
        &mut out,
    )?;

    Ok((packet_name, out))
}

fn capture_files(dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let mut files = Vec::new();

    if !dir.exists() {
        return Ok(files);
    }

    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();

        if path.extension().is_some_and(|ext| ext == "vpcap") {
            files.push(path);
        }
    }

    Ok(files)
}

fn side_map() -> HashMap<PacketSide, Vec<u8>> {
    HashMap::from([
        (PacketSide::Serverbound, Vec::new()),
        (PacketSide::Clientbound, Vec::new()),
    ])
}

fn coverage_pct(seen: usize, total: usize) -> f64 {
    if total == 0 {
        100.0
    } else {
        seen as f64 / total as f64 * 100.0
    }
}

#[derive(Default)]
struct ReplaySummary {
    total_records: usize,
    replayed_records: usize,
    failed_records: usize,
    seen_defined_packets: HashSet<(crate::PacketSide, crate::PacketState, i32)>,
}

impl ReplaySummary {
    fn merge(&mut self, other: &Self) {
        self.total_records += other.total_records;
        self.replayed_records += other.replayed_records;
        self.failed_records += other.failed_records;
        self.seen_defined_packets
            .extend(other.seen_defined_packets.iter().copied());
    }
}
