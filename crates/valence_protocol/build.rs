use heck::{ToPascalCase, ToSnakeCase};
use proc_macro2::TokenStream;
use quote::quote;
use serde::Deserialize;
use valence_build_utils::{ident, rerun_if_changed, write_generated_file};

#[derive(Deserialize)]
struct Packet {
    name: String,
    side: String,
    phase: String,
    id: i32,
}

fn main() -> anyhow::Result<()> {
    rerun_if_changed(["../valence_generated/extracted/packets.json"]);

    let packets: Vec<Packet> =
        serde_json::from_str(include_str!("../valence_generated/extracted/packets.json"))?;

    write_capture_replay_dispatch(&packets)
}

fn write_capture_replay_dispatch(packets: &[Packet]) -> anyhow::Result<()> {
    let packet_count = packets.len();
    let mut arms = TokenStream::new();
    let mut helpers = TokenStream::new();
    let mut known_arms = TokenStream::new();

    for packet in packets {
        let packet_id = packet.id;
        let module_ident = ident(packet.phase.to_snake_case());
        let type_ident = ident(normalized_packet_type_name(packet).to_pascal_case());
        let helper_ident = ident(format!(
            "replay_{}_{}_{}",
            packet.phase.to_snake_case(),
            packet.name.to_snake_case(),
            packet.side
        ));
        let side_ident = match packet.side.as_str() {
            "clientbound" => ident("Clientbound"),
            "serverbound" => ident("Serverbound"),
            _ => unreachable!(),
        };
        let state_ident = match packet.phase.as_str() {
            "handshake" => ident("Handshake"),
            "status" => ident("Status"),
            "login" => ident("Login"),
            "configuration" => ident("Configuration"),
            "play" => ident("Play"),
            _ => unreachable!(),
        };

        helpers.extend(quote! {
            fn #helper_ident(
                threshold: crate::CompressionThreshold,
                frame: &crate::decode::PacketFrame,
                out: &mut Vec<u8>,
            ) -> crate::anyhow::Result<&'static str> {
                use valence_binary::Decode;
                use crate::encode::WritePacket;
                use crate::Packet;

                let mut body = &frame.body[..];
                let packet = crate::packets::#module_ident::#type_ident::decode(&mut body)?;

                crate::anyhow::ensure!(
                    body.is_empty(),
                    "missed {} bytes while decoding '{}'",
                    body.len(),
                    crate::packets::#module_ident::#type_ident::NAME
                );

                let mut writer = crate::encode::PacketWriter::new(out, threshold);
                writer.write_packet_fallible(&packet)?;

                Ok(crate::packets::#module_ident::#type_ident::NAME)
            }
        });

        arms.extend(quote! {
            (crate::PacketSide::#side_ident, crate::PacketState::#state_ident, #packet_id) =>
                #helper_ident(threshold, frame, out),
        });

        known_arms.extend(quote! {
            (crate::PacketSide::#side_ident, crate::PacketState::#state_ident, #packet_id) => true,
        });
    }

    write_generated_file(
        quote! {
            pub(crate) const TOTAL_KNOWN_PACKETS: usize = #packet_count;

            #helpers

            pub(crate) fn is_known_packet(
                side: crate::PacketSide,
                state: crate::PacketState,
                packet_id: i32,
            ) -> bool {
                match (side, state, packet_id) {
                    #known_arms
                    _ => false,
                }
            }

            pub(crate) fn decode_and_reencode_known_packet(
                side: crate::PacketSide,
                state: crate::PacketState,
                threshold: crate::CompressionThreshold,
                frame: &crate::decode::PacketFrame,
                out: &mut Vec<u8>,
            ) -> crate::anyhow::Result<&'static str> {
                match (side, state, frame.id) {
                    #arms
                    _ => crate::anyhow::bail!(
                        "unknown packet for {:?}/{:?}: 0x{:02X}",
                        side,
                        state,
                        frame.id
                    ),
                }
            }
        },
        "capture_replay_dispatch.rs",
    )
}

fn normalized_packet_type_name(packet: &Packet) -> String {
    let name = packet.name.strip_suffix("Packet").unwrap_or(&packet.name);

    let name = if packet.side == "clientbound" && !name.ends_with("S2c") {
        format!("{name}S2c")
    } else if packet.side == "serverbound" && !name.ends_with("C2s") {
        format!("{name}C2s")
    } else {
        name.to_owned()
    };

    name
}
