use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::mem;
use std::sync::atomic::{AtomicU32, Ordering};

use parking_lot::Mutex; // Using nonstandard mutex to avoid poisoning API.
use valence_binary::Encode;
use valence_generated::block::{BlockKind, PropName, PropValue};
use valence_nbt::Compound;
use valence_protocol::encode::{PacketWriter, WritePacket};
use valence_protocol::packets::play::level_chunk_with_light_s2c::{
    ChunkDataBlockEntity, HeightMap, HeightMapKind,
};
use valence_protocol::packets::play::section_blocks_update_s2c::ChunkDeltaUpdateEntry;
use valence_protocol::packets::play::{
    BlockEntityDataS2c, BlockUpdateS2c, LevelChunkWithLightS2c, SectionBlocksUpdateS2c,
};
use valence_protocol::{BitStorage, BlockPos, BlockState, ChunkPos, ChunkSectionPos, FixedArray};
use valence_registry::biome::BiomeId;
use valence_registry::RegistryIdx;

use super::chunk::{
    bit_width, check_biome_oob, check_block_oob, check_section_oob, BiomeContainer,
    BlockStateContainer, Chunk, SECTION_BLOCK_COUNT,
};
use super::paletted_container::PalettedContainer;
use super::unloaded::{self, UnloadedChunk};
use super::{ChunkLayerInfo, ChunkLayerMessages, LocalMsg};

#[derive(Debug)]
pub struct LoadedChunk {
    /// A count of the clients viewing this chunk. Useful for knowing if it's
    /// necessary to record changes, since no client would be in view to receive
    /// the changes if this were zero.
    viewer_count: AtomicU32,
    /// Block and biome data for the chunk.
    sections: Box<[Section]>,
    /// Sky light data for the chunk. Light sections have one extra section at
    /// the top and bottom to account for skylight changes above and below the
    /// chunk.
    sky_light_sections: Box<[LightSection]>,
    /// Block light data for the chunk. Light sections have one extra section at
    /// the top and bottom to account for light changes above and below the
    /// chunk.
    block_light_sections: Box<[LightSection]>,
    /// The block entities in this chunk.
    block_entities: BTreeMap<u32, Compound>,
    /// The set of block entities that have been modified this tick.
    changed_block_entities: BTreeSet<u32>,
    /// If any biomes in this chunk have been modified this tick.
    changed_biomes: bool,
    /// Cached bytes of the chunk initialization packet. The cache is considered
    /// invalidated if empty. This should be cleared whenever the chunk is
    /// modified in an observable way, even if the chunk is not viewed.
    cached_init_packets: Mutex<Vec<u8>>,
}

#[derive(Clone, Debug, Default)]
pub struct Section {
    block_states: BlockStateContainer,
    biomes: BiomeContainer,
    /// Contains modifications for the update section packet. (Or the regular
    /// block update packet if len == 1).
    updates: Vec<ChunkDeltaUpdateEntry>,
}

impl Section {
    fn count_non_air_blocks(&self) -> u16 {
        let mut count = 0;

        match &self.block_states {
            PalettedContainer::Single(s) => {
                if !s.is_air() {
                    count += SECTION_BLOCK_COUNT as u16;
                }
            }
            PalettedContainer::Indirect(ind) => {
                for i in 0..SECTION_BLOCK_COUNT {
                    if !ind.get(i).is_air() {
                        count += 1;
                    }
                }
            }
            PalettedContainer::Direct(dir) => {
                for s in dir.as_ref() {
                    if !s.is_air() {
                        count += 1;
                    }
                }
            }
        }
        count
    }
}

/// Enum describing the light contents of a data section.
///
/// We need to differentiate between [`LightSection::NotSet`] and
/// [`LightSection::Single`].
/// This is because, for sky light, [`LightSection::NotSet`] could mean the
/// section is either fully lit or fully dark, and the client should deduce
/// that from the sky light data that is included.
#[derive(Clone, Debug, Default)]
pub enum LightSection {
    #[default]
    NotSet,
    Single(u8),
    FullData(Box<[u8; 2048]>),
}

impl LightSection {
    /// Create a new fully lit section of light data.
    pub fn with_full_light() -> Self {
        Self::Single(0xff)
    }

    /// Create a new fully dark (zeroed) section of light data.
    pub fn with_zeroed_light() -> Self {
        Self::Single(0x00)
    }

    /// Create a new section of light data with the given raw byte array
    pub fn from_data(data: [u8; 2048]) -> Self {
        Self::FullData(Box::new(data))
    }
}

impl LoadedChunk {
    pub(crate) fn new(height: u32) -> Self {
        let section_count = height as usize / 16;
        let light_section_count = section_count + 2;
        Self {
            viewer_count: AtomicU32::new(0),
            sections: vec![Section::default(); section_count].into(),
            sky_light_sections: vec![LightSection::default(); light_section_count].into(),
            // We don't have a full lighting engine implemented so we set all block light to be
            // fully dark.
            block_light_sections: vec![LightSection::with_zeroed_light(); light_section_count]
                .into(),
            block_entities: BTreeMap::new(),
            changed_block_entities: BTreeSet::new(),
            changed_biomes: false,
            cached_init_packets: Mutex::new(vec![]),
        }
    }

    /// Sets the content of this chunk to the supplied [`UnloadedChunk`]. The
    /// given unloaded chunk is [resized] to match the height of this loaded
    /// chunk prior to insertion.
    ///
    /// The previous chunk data is returned.
    ///
    /// [resized]: UnloadedChunk::set_height
    pub(crate) fn insert(&mut self, mut chunk: UnloadedChunk) -> UnloadedChunk {
        chunk.set_height(self.height());

        let old_sections = self
            .sections
            .iter_mut()
            .zip(chunk.sections)
            .map(|(sect, other_sect)| {
                sect.updates.clear();

                unloaded::Section {
                    block_states: mem::replace(&mut sect.block_states, other_sect.block_states),
                    biomes: mem::replace(&mut sect.biomes, other_sect.biomes),
                }
            })
            .collect();
        let old_block_entities = mem::replace(&mut self.block_entities, chunk.block_entities);
        self.changed_block_entities.clear();
        self.changed_biomes = false;
        self.cached_init_packets.get_mut().clear();
        self.assert_no_changes();

        UnloadedChunk {
            sections: old_sections,
            block_entities: old_block_entities,
        }
    }

    pub(crate) fn remove(&mut self) -> UnloadedChunk {
        let old_sections = self
            .sections
            .iter_mut()
            .map(|sect| {
                sect.updates.clear();

                unloaded::Section {
                    block_states: mem::take(&mut sect.block_states),
                    biomes: mem::take(&mut sect.biomes),
                }
            })
            .collect();
        let old_block_entities = mem::take(&mut self.block_entities);
        self.changed_block_entities.clear();
        self.changed_biomes = false;
        self.cached_init_packets.get_mut().clear();

        self.assert_no_changes();

        UnloadedChunk {
            sections: old_sections,
            block_entities: old_block_entities,
        }
    }

    /// Returns the number of clients in view of this chunk.
    pub fn viewer_count(&self) -> u32 {
        self.viewer_count.load(Ordering::Relaxed)
    }

    /// Like [`Self::viewer_count`], but avoids an atomic operation.
    pub fn viewer_count_mut(&mut self) -> u32 {
        *self.viewer_count.get_mut()
    }

    /// Increments the viewer count.
    pub(crate) fn inc_viewer_count(&self) {
        self.viewer_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Decrements the viewer count.
    #[track_caller]
    pub(crate) fn dec_viewer_count(&self) {
        let old = self.viewer_count.fetch_sub(1, Ordering::Relaxed);
        debug_assert_ne!(old, 0, "viewer count underflow!");
    }

    /// Performs the changes necessary to prepare this chunk for client updates.
    /// - Chunk change messages are written to the layer.
    /// - Recorded changes are cleared.
    pub(crate) fn update_pre_client(
        &mut self,
        pos: ChunkPos,
        info: &ChunkLayerInfo,
        messages: &mut ChunkLayerMessages,
    ) {
        if *self.viewer_count.get_mut() == 0 {
            // Nobody is viewing the chunk, so no need to send any update packets. There
            // also shouldn't be any changes that need to be cleared.
            self.assert_no_changes();

            return;
        }

        // Block states
        for (sect_y, sect) in self.sections.iter_mut().enumerate() {
            match sect.updates.as_slice() {
                &[] => {}
                &[entry] => {
                    let global_x = pos.x * 16 + i32::from(entry.off_x());
                    let global_y = info.min_y + sect_y as i32 * 16 + i32::from(entry.off_y());
                    let global_z = pos.z * 16 + i32::from(entry.off_z());

                    messages.send_local_infallible(LocalMsg::PacketAt { pos }, |buf| {
                        let mut writer = PacketWriter::new(buf, info.threshold);

                        writer.write_packet(&BlockUpdateS2c {
                            position: BlockPos::new(global_x, global_y, global_z),
                            block_id: BlockState::from_raw(entry.block_state() as u16).unwrap(),
                        });
                    });
                }
                entries => {
                    let chunk_sect_pos = ChunkSectionPos {
                        x: pos.x,
                        y: sect_y as i32 + info.min_y.div_euclid(16),
                        z: pos.z,
                    };

                    messages.send_local_infallible(LocalMsg::PacketAt { pos }, |buf| {
                        let mut writer = PacketWriter::new(buf, info.threshold);

                        writer.write_packet(&SectionBlocksUpdateS2c {
                            chunk_sect_pos,
                            blocks: Cow::Borrowed(entries),
                        });
                    });
                }
            }

            sect.updates.clear();
        }

        // Block entities
        for &idx in &self.changed_block_entities {
            let Some(nbt) = self.block_entities.get(&idx) else {
                continue;
            };

            let x = idx % 16;
            let z = (idx / 16) % 16;
            let y = idx / 16 / 16;

            let state = self.sections[y as usize / 16]
                .block_states
                .get(idx as usize % SECTION_BLOCK_COUNT);

            let Some(kind) = state.block_entity_kind() else {
                continue;
            };

            let global_x = pos.x * 16 + x as i32;
            let global_y = info.min_y + y as i32;
            let global_z = pos.z * 16 + z as i32;

            messages.send_local_infallible(LocalMsg::PacketAt { pos }, |buf| {
                let mut writer = PacketWriter::new(buf, info.threshold);

                writer.write_packet(&BlockEntityDataS2c {
                    location: BlockPos::new(global_x, global_y, global_z),
                    kind,
                    data: Cow::Borrowed(nbt),
                });
            });
        }

        self.changed_block_entities.clear();

        // Biomes
        if self.changed_biomes {
            self.changed_biomes = false;

            messages.send_local_infallible(LocalMsg::ChangeBiome { pos }, |buf| {
                for sect in &self.sections {
                    sect.biomes
                        .encode_mc_format(
                            &mut *buf,
                            |b| b.to_index() as u64,
                            0,
                            3,
                            bit_width(info.biome_registry_len - 1),
                        )
                        .expect("paletted container encode should always succeed");
                }
            });
        }

        // All changes should be cleared.
        self.assert_no_changes();
    }

    fn motion_blocking(&self) -> [u32; 16 * 16] {
        self.build_heightmap(Self::is_motion_blocking_occupied)
    }

    fn motion_blocking_no_leaves(&self) -> [u32; 16 * 16] {
        self.build_heightmap(Self::is_motion_blocking_no_leaves_occupied)
    }

    fn world_surface(&self) -> [u32; 16 * 16] {
        self.build_heightmap(|state| !state.is_air())
    }

    fn build_heightmap(&self, mut is_occupied: impl FnMut(BlockState) -> bool) -> [u32; 16 * 16] {
        let mut heightmap = [0; 16 * 16];

        for z in 0_u32..16 {
            for x in 0_u32..16 {
                for y in (0..self.height()).rev() {
                    if is_occupied(self.block_state(x, y, z)) {
                        // Heightmap values are 1-indexed local Y coordinates, where 0
                        // means "no occupied block in this column".
                        heightmap[(z as usize) * 16 + (x as usize)] = y + 1;
                        break;
                    }
                }
            }
        }

        heightmap
    }

    fn is_motion_blocking_occupied(state: BlockState) -> bool {
        let kind = state.to_kind();

        if matches!(kind, BlockKind::BambooSapling | BlockKind::Cactus) {
            return false;
        }

        state.blocks_motion()
            || state.is_liquid()
            || state.get(PropName::Waterlogged) == Some(PropValue::True)
    }

    fn is_motion_blocking_no_leaves_occupied(state: BlockState) -> bool {
        if Self::is_leaf_block(state) {
            return false;
        }

        Self::is_motion_blocking_occupied(state)
    }

    fn is_leaf_block(state: BlockState) -> bool {
        state.to_kind().to_str().ends_with("_leaves")
    }

    /// Encodes a given heightmap into the packed long-array format used in
    /// `LevelChunkWithLightS2c`.
    fn encode_heightmap(heightmap: &[u32; 16 * 16], world_height: u32) -> Vec<i64> {
        let bits_per_entry = (u32::BITS - world_height.leading_zeros()).max(1);
        let entries_per_long = i64::BITS / bits_per_entry;
        let longs_per_packet =
            (16 * 16) / entries_per_long + u32::from((16 * 16) % entries_per_long != 0);

        let mut data: Vec<i64> = vec![0; longs_per_packet as usize];

        for (idx, y) in heightmap.iter().enumerate() {
            debug_assert!(*y <= world_height);

            let long_idx = idx / entries_per_long as usize;
            let bit_offset = (idx % entries_per_long as usize) as u32 * bits_per_entry;
            data[long_idx] |= i64::from(*y) << bit_offset;
        }

        data
    }

    #[inline]
    fn light_idx(x: u32, y: u32, z: u32) -> usize {
        (x + z * 16 + y * 16 * 16) as usize
    }

    fn sky_light_filter_attenuation(state: BlockState) -> u8 {
        let kind = state.to_kind();

        let is_transparent_waterlogged =
            state.get(PropName::Waterlogged) == Some(PropValue::True) && !state.is_opaque();

        (state.is_liquid()
            || is_transparent_waterlogged
            || Self::is_leaf_block(state)
            || matches!(
                kind,
                BlockKind::BubbleColumn
                    | BlockKind::Ice
                    | BlockKind::FrostedIce
                    | BlockKind::Cobweb
                    | BlockKind::SlimeBlock
                    | BlockKind::HoneyBlock
                    | BlockKind::Spawner
                    | BlockKind::Beacon
                    | BlockKind::EndGateway
                    | BlockKind::ChorusPlant
                    | BlockKind::ChorusFlower
            )
            || kind.to_str().ends_with("_shulker_box"))
        .into()
    }

    fn calculate_sky_light_values(&self, world_surface: &[u32; 16 * 16]) -> Vec<u8> {
        let mut light = vec![0_u8; self.height() as usize * 16 * 16];
        let mut queue = VecDeque::new();

        for z in 0_u32..16 {
            for x in 0_u32..16 {
                let col_idx = (z * 16 + x) as usize;
                let world_surface_y = world_surface[col_idx].min(self.height());
                let mut vertical_light = 15_u8;

                for y in (world_surface_y..self.height()).rev() {
                    let idx = Self::light_idx(x, y, z);
                    if light[idx] < 15 {
                        light[idx] = 15;
                        queue.push_back((x, y, z));
                    }
                }

                for y in (0..world_surface_y).rev() {
                    let state = self.block_state(x, y, z);

                    if state.is_opaque() {
                        break;
                    }

                    let attenuation = if vertical_light == 15 {
                        Self::sky_light_filter_attenuation(state)
                    } else {
                        1
                    };
                    vertical_light = vertical_light.saturating_sub(attenuation);

                    if vertical_light == 0 {
                        break;
                    }

                    let idx = Self::light_idx(x, y, z);
                    if vertical_light > light[idx] {
                        light[idx] = vertical_light;
                        queue.push_back((x, y, z));
                    }
                }
            }
        }

        while let Some((x, y, z)) = queue.pop_front() {
            let current = light[Self::light_idx(x, y, z)];

            if current == 0 {
                continue;
            }

            for (dx, dy, dz) in [
                (-1_i32, 0_i32, 0_i32),
                (1, 0, 0),
                (0, -1, 0),
                (0, 1, 0),
                (0, 0, -1),
                (0, 0, 1),
            ] {
                let nx = x as i32 + dx;
                let ny = y as i32 + dy;
                let nz = z as i32 + dz;

                if !(0..16).contains(&nx) || !(0..16).contains(&nz) {
                    continue;
                }

                if !(0..self.height() as i32).contains(&ny) {
                    continue;
                }

                let nx = nx as u32;
                let ny = ny as u32;
                let nz = nz as u32;

                let neighbor_state = self.block_state(nx, ny, nz);

                if neighbor_state.is_opaque() {
                    continue;
                }

                let attenuation = if current == 15 && dy == -1 {
                    Self::sky_light_filter_attenuation(neighbor_state)
                } else {
                    1
                };
                let propagated = current.saturating_sub(attenuation);

                if propagated == 0 {
                    continue;
                }

                let nidx = Self::light_idx(nx, ny, nz);
                if propagated > light[nidx] {
                    light[nidx] = propagated;
                    queue.push_back((nx, ny, nz));
                }
            }
        }

        light
    }

    fn has_blocks_in_chunk_section(section_has_blocks: &[bool], section_y: isize) -> bool {
        section_y
            .try_into()
            .ok()
            .and_then(|idx: usize| section_has_blocks.get(idx))
            .copied()
            .unwrap_or(false)
    }

    fn calculate_sky_light_section(
        &self,
        section_idx: usize,
        sky_light_values: &[u8],
        section_has_blocks: &[bool],
        opaque_heightmap: &[u32; 16 * 16],
    ) -> LightSection {
        let chunk_section_y = section_idx as isize - 1;

        let section_has_no_blocks =
            !Self::has_blocks_in_chunk_section(section_has_blocks, chunk_section_y);
        let section_above_has_no_blocks =
            !Self::has_blocks_in_chunk_section(section_has_blocks, chunk_section_y + 1);

        if section_has_no_blocks && section_above_has_no_blocks {
            return LightSection::NotSet;
        }

        if chunk_section_y < 0 || chunk_section_y >= self.sections.len() as isize {
            return LightSection::with_zeroed_light();
        }

        let section_top_y = chunk_section_y as u32 * 16 + 15;
        if opaque_heightmap.iter().all(|&h| h > section_top_y) {
            return LightSection::with_zeroed_light();
        }

        let section_start_y = chunk_section_y as u32 * 16;
        let mut data = [0_u8; 2048];
        let mut first_value = 0_u8;
        let mut has_first_value = false;
        let mut is_uniform = true;
        let mut block_idx = 0_usize;

        for local_y in 0_u32..16 {
            for z in 0_u32..16 {
                for x in 0_u32..16 {
                    let y = section_start_y + local_y;
                    let light = sky_light_values[Self::light_idx(x, y, z)] & 0x0f;

                    if has_first_value {
                        is_uniform &= light == first_value;
                    } else {
                        first_value = light;
                        has_first_value = true;
                    }

                    let nibble_idx = block_idx / 2;
                    if block_idx.is_multiple_of(2) {
                        data[nibble_idx] = light;
                    } else {
                        data[nibble_idx] |= light << 4;
                    }

                    block_idx += 1;
                }
            }
        }

        if is_uniform {
            let value = first_value | (first_value << 4);
            LightSection::Single(value)
        } else {
            LightSection::from_data(data)
        }
    }

    fn calculate_sky_light_sections(&self, world_surface: &[u32; 16 * 16]) -> Box<[LightSection]> {
        let light_section_count = self.sections.len() + 2;
        let sky_light_values = self.calculate_sky_light_values(world_surface);
        let section_has_blocks: Vec<bool> = self
            .sections
            .iter()
            .map(|section| section.count_non_air_blocks() != 0)
            .collect();
        let opaque_heightmap = self.build_heightmap(|state| state.is_opaque());

        (0..light_section_count)
            .map(|i| {
                self.calculate_sky_light_section(
                    i,
                    &sky_light_values,
                    &section_has_blocks,
                    &opaque_heightmap,
                )
            })
            .collect()
    }

    fn fill_light_data(
        light: &LightSection,
        light_arrays: &mut Vec<FixedArray<u8, 2048>>,
        light_mask: &mut BitStorage,
        empty_light_mask: &mut BitStorage,
        i: usize,
        is_block_light: bool,
    ) {
        match light {
            LightSection::NotSet => {
                // For sky light, the client will deduce this section to be either fully lit or
                // fully dark based on the presence of light data in other light sections in the
                // chunk.
                if is_block_light {
                    empty_light_mask.set(i, 1);
                }
            }
            LightSection::Single(0x00) => {
                empty_light_mask.set(i, 1);
            }
            LightSection::Single(b) => {
                light_arrays.push(FixedArray([*b; 2048]));
                light_mask.set(i, 1);
            }
            LightSection::FullData(data) => {
                light_arrays.push(FixedArray(**data));
                light_mask.set(i, 1);
            }
        }
    }

    /// Writes the packet data needed to initialize this chunk.
    pub(crate) fn write_init_packets(
        &self,
        mut writer: impl WritePacket,
        pos: ChunkPos,
        info: &ChunkLayerInfo,
    ) {
        let mut init_packets = self.cached_init_packets.lock();

        if init_packets.is_empty() {
            let world_surface = self.world_surface();
            let motion_blocking = self.motion_blocking();
            let motion_blocking_no_leaves = self.motion_blocking_no_leaves();
            let world_height = self.height();

            // HACK: We don't have a full lighting engine implemented and we don't load
            // height maps or light from the world data. To avoid shrouding the
            // world in darkness, we calculate the sky light sections here from
            // scratch. What would also work is setting all sky light sections
            // to Single(0xff), but that uses a lot more ram if you have many chunks.
            // Currently, setting Single(0xff) for all sky light will start failing
            // many_players_spread_out benchmark due to OOM. So calculating the
            // sky light sections from scratch here, meaning we don't have to store sky
            // light for most sections since most sky light sections in a chunk
            // are NotSet (fully lit or fully dark).

            // This currently sky lighting implementation was based off the sky light
            // description on the wiki:
            //     - https://minecraft.wiki/w/Light#Sky_light
            //
            // The current implementation is incomplete in two main ways:
            // 1. We don't consider partial directional occlusion from blocks and we treat locks like that as fully light blocking.
            //     Real vanilla lighting uses face/shape occlusion between two neighboring blocks, not just “opaque or not”.
            //     https://minecraft.wiki/w/Light#:~:text=directional%20opacity
            //     So for blocks like slabs/stairs/path blocks:
            //         - They may block light in one direction but not others.
            //         - is_opaque() can’t express that per-face behavior.
            //         - Our current model treats them as either fully blocking or fully
            //           non-blocking, which is an approximation.
            //
            // 2. We don't calculate sky light across chunk boundaries. This is because we
            //    don't have the context here for other chunks. We approximate what how the
            //    sky light would propagate within the chunk itself, but we don't consider
            //    how neighboring chunks might affect the sky light at the edges of the
            //    chunk.
            //
            //     Vanilla handles this by running a chunk-light graph update across chunk
            // boundaries:
            //         - It stores light in a global light engine (not isolated per chunk
            //           packet build).
            //         - When chunks load/change, it enqueues light updates.
            //         - Propagation crosses chunk edges into loaded neighbors.
            //         - If a neighbor isn’t loaded yet, updates are deferred/continued when
            //           that chunk becomes available.
            //         - Network packets then send changed light sections to clients
            //           incrementally (LightUpdate), instead of recomputing a chunk in
            //           isolation each time.

            let calculated_sky_light_sections = self.calculate_sky_light_sections(&world_surface);

            let heightmaps = vec![
                HeightMap {
                    kind: HeightMapKind::WorldSurface,
                    data: LoadedChunk::encode_heightmap(&world_surface, world_height),
                },
                HeightMap {
                    kind: HeightMapKind::MotionBlocking,
                    data: LoadedChunk::encode_heightmap(&motion_blocking, world_height),
                },
                HeightMap {
                    kind: HeightMapKind::MotionBlockingNoLeaves,
                    data: LoadedChunk::encode_heightmap(&motion_blocking_no_leaves, world_height),
                },
            ];

            let mut blocks_and_biomes: Vec<u8> = vec![];

            let light_section_count = self.sections.len() + 2;

            let mut sky_light_mask = BitStorage::new(1, light_section_count, None).unwrap();
            let mut empty_sky_light_mask = BitStorage::new(1, light_section_count, None).unwrap();
            let mut block_light_mask = BitStorage::new(1, light_section_count, None).unwrap();
            let mut empty_block_light_mask = BitStorage::new(1, light_section_count, None).unwrap();

            let mut sky_light_arrays = Vec::with_capacity(light_section_count);
            let mut block_light_arrays = Vec::with_capacity(light_section_count);

            for (i, sky_light_override) in self.sky_light_sections.iter().enumerate() {
                let sky_light_calculated = &calculated_sky_light_sections[i];
                let sky_light = if matches!(sky_light_override, LightSection::NotSet) {
                    sky_light_calculated
                } else {
                    sky_light_override
                };

                LoadedChunk::fill_light_data(
                    sky_light,
                    &mut sky_light_arrays,
                    &mut sky_light_mask,
                    &mut empty_sky_light_mask,
                    i,
                    false,
                );
            }

            for (i, block_light) in self.block_light_sections.iter().enumerate() {
                LoadedChunk::fill_light_data(
                    block_light,
                    &mut block_light_arrays,
                    &mut block_light_mask,
                    &mut empty_block_light_mask,
                    i,
                    true,
                );
            }

            for sect in &self.sections {
                sect.count_non_air_blocks()
                    .encode(&mut blocks_and_biomes)
                    .unwrap();

                sect.block_states
                    .encode_mc_format(
                        &mut blocks_and_biomes,
                        |b| b.to_raw().into(),
                        4,
                        8,
                        bit_width(BlockState::max_raw().into()),
                    )
                    .expect("paletted container encode should always succeed");

                sect.biomes
                    .encode_mc_format(
                        &mut blocks_and_biomes,
                        |b| b.to_index() as u64,
                        0,
                        3,
                        bit_width(info.biome_registry_len - 1),
                    )
                    .expect("paletted container encode should always succeed");
            }

            let block_entities: Vec<_> = self
                .block_entities
                .iter()
                .filter_map(|(&idx, nbt)| {
                    let x = idx % 16;
                    let z = idx / 16 % 16;
                    let y = idx / 16 / 16;

                    let kind = self.sections[y as usize / 16]
                        .block_states
                        .get(idx as usize % SECTION_BLOCK_COUNT)
                        .block_entity_kind();

                    kind.map(|kind| ChunkDataBlockEntity {
                        packed_xz: ((x << 4) | z) as i8,
                        y: y as i16 + info.min_y as i16,
                        kind,
                        data: Cow::Borrowed(nbt),
                    })
                })
                .collect();

            PacketWriter::new(&mut init_packets, info.threshold).write_packet(
                &LevelChunkWithLightS2c {
                    pos,
                    heightmaps: Cow::Owned(heightmaps),
                    blocks_and_biomes: &blocks_and_biomes,
                    block_entities: Cow::Owned(block_entities),
                    sky_light_mask: Cow::Borrowed(&sky_light_mask.into_data()),
                    block_light_mask: Cow::Borrowed(&block_light_mask.into_data()),
                    empty_sky_light_mask: Cow::Borrowed(&empty_sky_light_mask.into_data()),
                    empty_block_light_mask: Cow::Borrowed(&empty_block_light_mask.into_data()),
                    sky_light_arrays: Cow::Borrowed(&sky_light_arrays),
                    block_light_arrays: Cow::Borrowed(&block_light_arrays),
                },
            )
        }

        writer.write_packet_bytes(&init_packets);
    }

    /// Asserts that no changes to this chunk are currently recorded.
    #[track_caller]
    fn assert_no_changes(&self) {
        #[cfg(debug_assertions)]
        {
            assert!(!self.changed_biomes);
            assert!(self.changed_block_entities.is_empty());

            for sect in &self.sections {
                assert!(sect.updates.is_empty());
            }
        }
    }
}

impl Chunk for LoadedChunk {
    fn height(&self) -> u32 {
        self.sections.len() as u32 * 16
    }

    fn block_state(&self, x: u32, y: u32, z: u32) -> BlockState {
        check_block_oob(self, x, y, z);

        let idx = x + z * 16 + y % 16 * 16 * 16;
        self.sections[y as usize / 16]
            .block_states
            .get(idx as usize)
    }

    fn set_block_state(&mut self, x: u32, y: u32, z: u32, block: BlockState) -> BlockState {
        check_block_oob(self, x, y, z);

        let sect_y = y / 16;
        let sect = &mut self.sections[sect_y as usize];
        let idx = x + z * 16 + y % 16 * 16 * 16;

        let old_block = sect.block_states.set(idx as usize, block);

        if block != old_block {
            self.cached_init_packets.get_mut().clear();

            if *self.viewer_count.get_mut() > 0 {
                sect.updates.push(
                    ChunkDeltaUpdateEntry::new()
                        .with_off_x(x as u8)
                        .with_off_y((y % 16) as u8)
                        .with_off_z(z as u8)
                        .with_block_state(block.to_raw().into()),
                );
            }
        }

        old_block
    }

    fn fill_block_state_section(&mut self, sect_y: u32, block: BlockState) {
        check_section_oob(self, sect_y);

        let sect = &mut self.sections[sect_y as usize];

        if let PalettedContainer::Single(b) = &sect.block_states {
            if *b != block {
                self.cached_init_packets.get_mut().clear();

                if *self.viewer_count.get_mut() > 0 {
                    // The whole section is being modified, so any previous modifications would
                    // be overwritten.
                    sect.updates.clear();

                    // Push section updates for all the blocks in the section.
                    sect.updates.reserve_exact(SECTION_BLOCK_COUNT);
                    for z in 0..16 {
                        for x in 0..16 {
                            for y in 0..16 {
                                sect.updates.push(
                                    ChunkDeltaUpdateEntry::new()
                                        .with_off_x(x)
                                        .with_off_y(y)
                                        .with_off_z(z)
                                        .with_block_state(block.to_raw().into()),
                                );
                            }
                        }
                    }
                }
            }
        } else {
            for z in 0..16 {
                for x in 0..16 {
                    for y in 0..16 {
                        let idx = x + z * 16 + (sect_y * 16 + y) * (16 * 16);

                        if block != sect.block_states.get(idx as usize) {
                            self.cached_init_packets.get_mut().clear();

                            if *self.viewer_count.get_mut() > 0 {
                                sect.updates.push(
                                    ChunkDeltaUpdateEntry::new()
                                        .with_off_x(x as u8)
                                        .with_off_y(y as u8)
                                        .with_off_z(z as u8)
                                        .with_block_state(block.to_raw().into()),
                                );
                            }
                        }
                    }
                }
            }
        }

        sect.block_states.fill(block);
    }

    fn block_entity(&self, x: u32, y: u32, z: u32) -> Option<&Compound> {
        check_block_oob(self, x, y, z);

        let idx = x + z * 16 + y * 16 * 16;
        self.block_entities.get(&idx)
    }

    fn block_entity_mut(&mut self, x: u32, y: u32, z: u32) -> Option<&mut Compound> {
        check_block_oob(self, x, y, z);

        let idx = x + z * 16 + y * 16 * 16;

        if let Some(be) = self.block_entities.get_mut(&idx) {
            if *self.viewer_count.get_mut() > 0 {
                self.changed_block_entities.insert(idx);
            }
            self.cached_init_packets.get_mut().clear();

            Some(be)
        } else {
            None
        }
    }

    fn set_block_entity(
        &mut self,
        x: u32,
        y: u32,
        z: u32,
        block_entity: Option<Compound>,
    ) -> Option<Compound> {
        check_block_oob(self, x, y, z);

        let idx = x + z * 16 + y * 16 * 16;

        match block_entity {
            Some(nbt) => {
                if *self.viewer_count.get_mut() > 0 {
                    self.changed_block_entities.insert(idx);
                }
                self.cached_init_packets.get_mut().clear();

                self.block_entities.insert(idx, nbt)
            }
            None => {
                let res = self.block_entities.remove(&idx);

                if res.is_some() {
                    self.cached_init_packets.get_mut().clear();
                }

                res
            }
        }
    }

    fn clear_block_entities(&mut self) {
        if self.block_entities.is_empty() {
            return;
        }

        self.cached_init_packets.get_mut().clear();

        if *self.viewer_count.get_mut() > 0 {
            self.changed_block_entities
                .extend(mem::take(&mut self.block_entities).into_keys());
        } else {
            self.block_entities.clear();
        }
    }

    fn biome(&self, x: u32, y: u32, z: u32) -> BiomeId {
        check_biome_oob(self, x, y, z);

        let idx = x + z * 4 + y % 4 * 4 * 4;
        self.sections[y as usize / 4].biomes.get(idx as usize)
    }

    fn set_biome(&mut self, x: u32, y: u32, z: u32, biome: BiomeId) -> BiomeId {
        check_biome_oob(self, x, y, z);

        let idx = x + z * 4 + y % 4 * 4 * 4;
        let old_biome = self.sections[y as usize / 4]
            .biomes
            .set(idx as usize, biome);

        if biome != old_biome {
            self.cached_init_packets.get_mut().clear();

            if *self.viewer_count.get_mut() > 0 {
                self.changed_biomes = true;
            }
        }

        old_biome
    }

    fn fill_biome_section(&mut self, sect_y: u32, biome: BiomeId) {
        check_section_oob(self, sect_y);

        let sect = &mut self.sections[sect_y as usize];

        if let PalettedContainer::Single(b) = &sect.biomes {
            if *b != biome {
                self.cached_init_packets.get_mut().clear();
                self.changed_biomes = *self.viewer_count.get_mut() > 0;
            }
        } else {
            self.cached_init_packets.get_mut().clear();
            self.changed_biomes = *self.viewer_count.get_mut() > 0;
        }

        sect.biomes.fill(biome);
    }

    fn shrink_to_fit(&mut self) {
        self.cached_init_packets.get_mut().shrink_to_fit();

        for sect in &mut self.sections {
            sect.block_states.shrink_to_fit();
            sect.biomes.shrink_to_fit();
            sect.updates.shrink_to_fit();
        }
    }
}

#[cfg(test)]
mod tests {
    use valence_nbt::compound;
    use valence_protocol::CompressionThreshold;
    use valence_registry::dimension_type::DimensionTypeId;

    use super::*;

    fn heightmap_idx(x: usize, z: usize) -> usize {
        z * 16 + x
    }

    fn decode_heightmap(data: &[i64], bits_per_entry: u32) -> [u32; 16 * 16] {
        let entries_per_long = i64::BITS / bits_per_entry;
        let mask = (1_u64 << bits_per_entry) - 1;
        let mut decoded = [0; 16 * 16];

        for (idx, value) in decoded.iter_mut().enumerate() {
            let long_idx = idx / entries_per_long as usize;
            let bit_offset = (idx % entries_per_long as usize) as u32 * bits_per_entry;
            *value = ((data[long_idx] as u64 >> bit_offset) & mask) as u32;
        }

        decoded
    }

    #[test]
    fn loaded_chunk_unviewed_no_changes() {
        let mut chunk = LoadedChunk::new(512);

        chunk.set_block(0, 10, 0, BlockState::MAGMA_BLOCK);
        chunk.assert_no_changes();

        chunk.set_biome(0, 0, 0, BiomeId::from_index(5));
        chunk.assert_no_changes();

        chunk.fill_block_states(BlockState::ACACIA_BUTTON);
        chunk.assert_no_changes();

        chunk.fill_biomes(BiomeId::from_index(42));
        chunk.assert_no_changes();
    }

    #[test]
    fn loaded_chunk_changes_clear_packet_cache() {
        #[track_caller]
        fn check<T>(chunk: &mut LoadedChunk, change: impl FnOnce(&mut LoadedChunk) -> T) {
            let info = ChunkLayerInfo {
                dimension_type: DimensionTypeId::new(0),
                height: 512,
                min_y: -16,
                biome_registry_len: 200,
                threshold: CompressionThreshold(-1),
            };

            let mut buf = vec![];
            let mut writer = PacketWriter::new(&mut buf, CompressionThreshold(-1));

            // Rebuild cache.
            chunk.write_init_packets(&mut writer, ChunkPos::new(3, 4), &info);

            // Check that the cache is built.
            assert!(!chunk.cached_init_packets.get_mut().is_empty());

            // Making a change should clear the cache.
            change(chunk);
            assert!(chunk.cached_init_packets.get_mut().is_empty());

            // Rebuild cache again.
            chunk.write_init_packets(&mut writer, ChunkPos::new(3, 4), &info);
            assert!(!chunk.cached_init_packets.get_mut().is_empty());
        }

        let mut chunk = LoadedChunk::new(512);

        check(&mut chunk, |c| {
            c.set_block_state(0, 4, 0, BlockState::ACACIA_WOOD)
        });
        check(&mut chunk, |c| c.set_biome(1, 2, 3, BiomeId::from_index(4)));
        check(&mut chunk, |c| c.fill_biomes(BiomeId::DEFAULT));
        check(&mut chunk, |c| c.fill_block_states(BlockState::WET_SPONGE));
        check(&mut chunk, |c| {
            c.set_block_entity(3, 40, 5, Some(compound! {}))
        });
        check(&mut chunk, |c| {
            c.block_entity_mut(3, 40, 5).unwrap();
        });
        check(&mut chunk, |c| c.set_block_entity(3, 40, 5, None));

        // Old block state is the same as new block state, so the cache should still be
        // intact.
        assert_eq!(
            chunk.set_block_state(0, 0, 0, BlockState::WET_SPONGE),
            BlockState::WET_SPONGE
        );

        assert!(!chunk.cached_init_packets.get_mut().is_empty());
    }

    #[test]
    fn heightmap_occupancy_rules() {
        // Based on: https://minecraft.wiki/w/Java_Edition_protocol/Chunk_format#Heightmap_structure
        let mut chunk = LoadedChunk::new(32);

        chunk.set_block_state(0, 0, 0, BlockState::STONE);
        chunk.set_block_state(1, 5, 0, BlockState::OAK_LEAVES);
        chunk.set_block_state(2, 6, 0, BlockState::CACTUS);
        chunk.set_block_state(3, 7, 0, BlockState::WATER);
        chunk.set_block_state(
            4,
            8,
            0,
            BlockState::OAK_LEAVES.set(PropName::Waterlogged, PropValue::True),
        );

        let world_surface = chunk.world_surface();
        let motion_blocking = chunk.motion_blocking();
        let motion_blocking_no_leaves = chunk.motion_blocking_no_leaves();

        assert_eq!(world_surface[heightmap_idx(0, 0)], 1);
        assert_eq!(world_surface[heightmap_idx(1, 0)], 6);
        assert_eq!(world_surface[heightmap_idx(2, 0)], 7);
        assert_eq!(world_surface[heightmap_idx(3, 0)], 8);
        assert_eq!(world_surface[heightmap_idx(4, 0)], 9);

        assert_eq!(motion_blocking[heightmap_idx(0, 0)], 1);
        assert_eq!(motion_blocking[heightmap_idx(1, 0)], 6);
        assert_eq!(motion_blocking[heightmap_idx(2, 0)], 0);
        assert_eq!(motion_blocking[heightmap_idx(3, 0)], 8);
        assert_eq!(motion_blocking[heightmap_idx(4, 0)], 9);

        assert_eq!(motion_blocking_no_leaves[heightmap_idx(0, 0)], 1);
        assert_eq!(motion_blocking_no_leaves[heightmap_idx(1, 0)], 0);
        assert_eq!(motion_blocking_no_leaves[heightmap_idx(2, 0)], 0);
        assert_eq!(motion_blocking_no_leaves[heightmap_idx(3, 0)], 8);
        assert_eq!(motion_blocking_no_leaves[heightmap_idx(4, 0)], 0);
    }

    #[test]
    fn encode_heightmap_uses_dynamic_bit_width() {
        let mut chunk = LoadedChunk::new(512);
        chunk.set_block_state(0, 511, 0, BlockState::STONE);

        let motion_blocking = chunk.motion_blocking();
        assert_eq!(motion_blocking[heightmap_idx(0, 0)], 512);

        let encoded = LoadedChunk::encode_heightmap(&motion_blocking, chunk.height());
        // 512 world height => ceil(log2(512 + 1)) = 10 bits, so 64/10 = 6 entries per
        // long.
        assert_eq!(encoded.len(), 43);

        let decoded = decode_heightmap(&encoded, 10);
        assert_eq!(decoded[heightmap_idx(0, 0)], 512);
        assert_eq!(decoded[heightmap_idx(1, 0)], 0);
    }

    #[test]
    fn skylight_filtering_blocks_attenuate_vertical_light() {
        let mut chunk = LoadedChunk::new(32);

        chunk.fill_block_states(BlockState::STONE);
        for y in 0..31 {
            chunk.set_block_state(0, y, 0, BlockState::AIR);
        }
        chunk.set_block_state(0, 31, 0, BlockState::WATER);

        let world_surface = chunk.world_surface();
        let sky_light = chunk.calculate_sky_light_values(&world_surface);

        assert_eq!(sky_light[LoadedChunk::light_idx(0, 31, 0)], 14);
        assert_eq!(sky_light[LoadedChunk::light_idx(0, 30, 0)], 13);
        assert_eq!(sky_light[LoadedChunk::light_idx(1, 31, 0)], 0);
    }

    #[test]
    fn skylight_sections_optimization_notset_and_underground_zero() {
        let chunk = LoadedChunk::new(32);
        let world_surface = chunk.world_surface();
        let sky_light_sections = chunk.calculate_sky_light_sections(&world_surface);

        assert!(sky_light_sections
            .iter()
            .all(|section| matches!(section, LightSection::NotSet)));

        let mut chunk = LoadedChunk::new(32);
        chunk.fill_block_state_section(1, BlockState::STONE);

        let world_surface = chunk.world_surface();
        let sky_light_sections = chunk.calculate_sky_light_sections(&world_surface);

        assert!(matches!(sky_light_sections[0], LightSection::NotSet));
        assert!(matches!(sky_light_sections[1], LightSection::Single(0x00)));
        assert!(matches!(sky_light_sections[2], LightSection::Single(0x00)));
        assert!(matches!(sky_light_sections[3], LightSection::NotSet));
    }
}
