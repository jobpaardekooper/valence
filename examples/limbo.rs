#![allow(clippy::type_complexity)]

use std::collections::VecDeque;
use std::env;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use rand::seq::SliceRandom;
use rand::Rng;
use valence::protocol::sound::{Sound, SoundCategory};
use valence::spawn::IsFlat;
use valence::{prelude::*, MINECRAFT_VERSION, PROTOCOL_VERSION};
use valence_network::{async_trait, HandshakeData, ServerListPing};
use valence_scoreboard::{Objective, ObjectiveBundle, ObjectiveDisplay, ObjectiveScores};
use valence_text::color::NamedColor::{Red, Yellow};

const START_POS: BlockPos = BlockPos::new(0, 100, 0);
const VIEW_DIST: u8 = 10;

const BLOCK_TYPES: [BlockState; 7] = [
    BlockState::GRASS_BLOCK,
    BlockState::OAK_LOG,
    BlockState::BIRCH_LOG,
    BlockState::OAK_LEAVES,
    BlockState::BIRCH_LEAVES,
    BlockState::DIRT,
    BlockState::MOSS_BLOCK,
];

pub fn main() {
    let proxy_secret = env::var("PROXY_SECRET");

    let limbo_port = env::var("LIMBO_PORT");
    let port_number = limbo_port
        .as_deref()
        .unwrap_or("25565")
        .parse::<u16>()
        .unwrap_or(25565);

    println!("Starting Limbo on port {port_number}...");

    App::new()
        .insert_resource(NetworkSettings {
            callbacks: CustomNetworkCallbacks.into(),
            max_connections: 1024,
            max_players: 1024,
            address: SocketAddr::from(([0, 0, 0, 0], port_number)),
            connection_mode: match proxy_secret {
                Ok(secret) => match secret {
                    s if s.is_empty() => ConnectionMode::Online {
                        prevent_proxy_connections: false,
                    },
                    _ => ConnectionMode::Velocity {
                        secret: Arc::from(secret),
                    },
                },
                Err(_) => ConnectionMode::Online {
                    prevent_proxy_connections: false,
                },
            },
            ..Default::default()
        })
        .add_plugins(DefaultPlugins)
        .add_systems(
            Update,
            (
                init_clients,
                reset_clients.after(init_clients),
                manage_chunks.after(reset_clients).before(manage_blocks),
                manage_blocks,
                despawn_disconnected_clients,
                cleanup_disconnected_scoreboards.after(despawn_disconnected_clients),
            ),
        )
        .run();
}

struct CustomNetworkCallbacks;

#[async_trait]
impl NetworkCallbacks for CustomNetworkCallbacks {
    async fn server_list_ping(
        &self,
        _shared: &SharedNetworkState,
        _remote_addr: SocketAddr,
        _handshake_data: &HandshakeData,
    ) -> ServerListPing {
        let max_players = 0;

        ServerListPing::Respond {
            online_players: 0,
            max_players,
            player_sample: vec![],
            description: "Limbo".into_text(),
            favicon_png: &[],
            version_name: MINECRAFT_VERSION.to_owned(),
            protocol: PROTOCOL_VERSION,
        }
    }
}

#[derive(Component)]
struct GameState {
    blocks: VecDeque<BlockPos>,
    score: u32,
    high_score: u32,
    combo: u32,
    target_y: i32,
    last_block_timestamp: u128,
    scoreboard_objective: Entity,
}

#[derive(Component)]
struct ScoreboardOwner(Entity);

const HIGH_SCORE_LABEL: &str = "";

fn high_score_scores(high_score: u32) -> ObjectiveScores {
    ObjectiveScores::with_map([(HIGH_SCORE_LABEL.to_owned(), high_score as i32)])
}

fn init_clients(
    mut clients: Query<
        (
            Entity,
            &mut Client,
            &mut VisibleChunkLayer,
            &mut VisibleEntityLayers,
            &mut IsFlat,
            &mut GameMode,
        ),
        Added<Client>,
    >,
    server: Res<Server>,
    dimensions: Res<DimensionTypeRegistry>,
    biomes: Res<BiomeRegistry>,
    mut commands: Commands,
) {
    for (
        entity,
        mut client,
        mut visible_chunk_layer,
        mut visible_entity_layers,
        mut is_flat,
        mut game_mode,
    ) in &mut clients
    {
        visible_chunk_layer.0 = entity;
        is_flat.0 = true;
        *game_mode = GameMode::Adventure;

        client.send_chat_message("Welcome to the libmo!".color(Yellow));
        client.send_chat_message("You will be automatically reconnected to the server you tried to join once it becomes available.".color(Red).bold());

        let scoreboard_layer = commands
            .spawn((EntityLayer::new(&server), ScoreboardOwner(entity)))
            .id();
        visible_entity_layers.0.insert(scoreboard_layer);

        let scoreboard_objective = commands
            .spawn((
                ScoreboardOwner(entity),
                ObjectiveBundle {
                    name: Objective::new("limbo-high"),
                    display: ObjectiveDisplay("High Score".into_text()),
                    scores: high_score_scores(0),
                    layer: EntityLayerId(scoreboard_layer),
                    ..Default::default()
                },
            ))
            .id();

        let state = GameState {
            blocks: VecDeque::new(),
            score: 0,
            high_score: 0,
            combo: 0,
            target_y: 0,
            last_block_timestamp: 0,
            scoreboard_objective,
        };

        let layer = ChunkLayer::new(ident!("overworld"), &dimensions, &biomes, &server);

        commands.entity(entity).insert((state, layer));
    }
}

fn reset_clients(
    mut clients: Query<(
        &mut Client,
        &mut Position,
        &mut Look,
        &mut GameState,
        &mut ChunkLayer,
    )>,
    mut objective_scores: Query<&mut ObjectiveScores>,
) {
    for (mut client, mut pos, mut look, mut state, mut layer) in &mut clients {
        let out_of_bounds = (pos.0.y as i32) < START_POS.y - 32;

        if out_of_bounds || state.is_added() {
            if out_of_bounds && !state.is_added() {
                client.send_chat_message(
                    "Your score was ".italic()
                        + state
                            .score
                            .to_string()
                            .color(Color::GOLD)
                            .bold()
                            .not_italic(),
                );
            }

            state.high_score = state.high_score.max(state.score);
            update_high_score_objective(&state, &mut objective_scores);

            // Init chunks.
            for pos in ChunkView::new(START_POS.into(), VIEW_DIST).iter() {
                layer.insert_chunk(pos, UnloadedChunk::new());
            }

            state.score = 0;
            state.combo = 0;

            for block in &state.blocks {
                layer.set_block(*block, BlockState::AIR);
            }
            state.blocks.clear();
            state.blocks.push_back(START_POS);
            layer.set_block(START_POS, BlockState::STONE);

            for _ in 0..10 {
                generate_next_block(&mut state, &mut layer, false);
            }

            pos.set([
                f64::from(START_POS.x) + 0.5,
                f64::from(START_POS.y) + 1.0,
                f64::from(START_POS.z) + 0.5,
            ]);
            look.yaw = 0.0;
            look.pitch = 0.0;
        }
    }
}

fn manage_blocks(
    mut clients: Query<(&mut Client, &Position, &mut GameState, &mut ChunkLayer)>,
    mut objective_scores: Query<&mut ObjectiveScores>,
) {
    for (mut client, pos, mut state, mut layer) in &mut clients {
        let pos_under_player = BlockPos::new(
            (pos.0.x - 0.5).round() as i32,
            pos.0.y as i32 - 1,
            (pos.0.z - 0.5).round() as i32,
        );

        if let Some(index) = state
            .blocks
            .iter()
            .position(|block| *block == pos_under_player)
        {
            if index > 0 {
                let power_result = 2_f32.powf((state.combo as f32) / 45.0);
                let max_time_taken = (1000_f32 * (index as f32) / power_result) as u128;

                let current_time_millis = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_millis();

                if current_time_millis - state.last_block_timestamp < max_time_taken {
                    state.combo += index as u32
                } else {
                    state.combo = 0
                }

                for _ in 0..index {
                    generate_next_block(&mut state, &mut layer, true)
                }

                if state.score > state.high_score {
                    state.high_score = state.score;
                    update_high_score_objective(&state, &mut objective_scores);
                }

                let pitch = 0.9 + ((state.combo as f32) - 1.0) * 0.05;
                client.play_sound(
                    Sound::BlockNoteBlockBass,
                    SoundCategory::Master,
                    pos.0,
                    1.0,
                    pitch,
                );

                client.set_title("");
                client.set_subtitle(state.score.to_string().color(Color::LIGHT_PURPLE).bold());
            }
        }
    }
}

fn update_high_score_objective(
    state: &GameState,
    objective_scores: &mut Query<&mut ObjectiveScores>,
) {
    if let Ok(mut scores) = objective_scores.get_mut(state.scoreboard_objective) {
        scores.insert(HIGH_SCORE_LABEL, state.high_score as i32);
    }
}

fn cleanup_disconnected_scoreboards(
    mut commands: Commands,
    scoreboards: Query<(Entity, &ScoreboardOwner)>,
    clients: Query<(), With<Client>>,
) {
    for (entity, owner) in &scoreboards {
        if clients.get(owner.0).is_err() {
            commands.entity(entity).despawn();
        }
    }
}

fn manage_chunks(mut clients: Query<(&Position, &OldPosition, &mut ChunkLayer), With<Client>>) {
    for (pos, old_pos, mut layer) in &mut clients {
        let old_view = ChunkView::new(old_pos.get().into(), VIEW_DIST);
        let view = ChunkView::new(pos.0.into(), VIEW_DIST);

        if old_view != view {
            for pos in old_view.diff(view) {
                layer.remove_chunk(pos);
            }

            for pos in view.diff(old_view) {
                layer.chunk_entry(pos).or_default();
            }
        }
    }
}

fn generate_next_block(state: &mut GameState, layer: &mut ChunkLayer, in_game: bool) {
    if in_game {
        let removed_block = state.blocks.pop_front().unwrap();
        layer.set_block(removed_block, BlockState::AIR);

        state.score += 1
    }

    let last_pos = *state.blocks.back().unwrap();
    let block_pos = generate_random_block(last_pos, state.target_y);

    if last_pos.y == START_POS.y {
        state.target_y = 0
    } else if last_pos.y < START_POS.y - 30 || last_pos.y > START_POS.y + 30 {
        state.target_y = START_POS.y;
    }

    let mut rng = rand::thread_rng();

    layer.set_block(block_pos, *BLOCK_TYPES.choose(&mut rng).unwrap());
    state.blocks.push_back(block_pos);

    // Combo System
    state.last_block_timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis();
}

fn generate_random_block(pos: BlockPos, target_y: i32) -> BlockPos {
    let mut rng = rand::thread_rng();

    // if above or below target_y, change y to gradually reach it
    let y = match target_y {
        0 => rng.gen_range(-1..2),
        y if y > pos.y => 1,
        _ => -1,
    };
    let z = match y {
        1 => rng.gen_range(1..3),
        -1 => rng.gen_range(2..5),
        _ => rng.gen_range(1..4),
    };
    let x = rng.gen_range(-3..4);

    BlockPos::new(pos.x + x, pos.y + y, pos.z + z)
}
