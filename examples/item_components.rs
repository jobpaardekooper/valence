use valence::entity::attributes::EntityAttributeOperation;
use valence::inventory::ClickSlotEvent;
use valence::item::{
    AttributeModifier, AttributeSlot, ConsumableAnimation, EquipSlot, ItemComponent,
    ResolvableProfile, SoundEventDefinition,
};
use valence::prelude::*;
use valence::protocol::text::TextContent;
use valence::protocol::IntoTextComponent;
use valence_binary::{IdOr, VarInt};

fn main() {
    App::new()
        .add_plugins(DefaultPlugins)
        .add_systems(Startup, setup)
        .add_systems(Update, (init_clients, despawn_disconnected_clients))
        .add_systems(
            Update,
            (
                init_clients,
                report_probe_clicks,
                despawn_disconnected_clients,
            ),
        )
        .run();
}

fn setup(
    mut commands: Commands,
    server: Res<Server>,
    dimensions: Res<DimensionTypeRegistry>,
    biomes: Res<BiomeRegistry>,
) {
    let mut layer = LayerBundle::new(ident!("overworld"), &dimensions, &biomes, &server);

    for z in -5..5 {
        for x in -5..5 {
            layer.chunk.insert_chunk([x, z], UnloadedChunk::new());
        }
    }

    for z in -25..25 {
        for x in -25..25 {
            layer.chunk.set_block([x, 64, z], BlockState::GRASS_BLOCK);
        }
    }

    commands.spawn(layer);
}

fn init_clients(
    mut clients: Query<
        (
            &mut EntityLayerId,
            &mut VisibleChunkLayer,
            &mut VisibleEntityLayers,
            &mut Position,
            &mut GameMode,
            &mut Inventory,
            &mut Client,
        ),
        Added<Client>,
    >,
    layers: Query<Entity, With<ChunkLayer>>,
) {
    for (
        mut layer_id,
        mut visible_chunk_layer,
        mut visible_entity_layers,
        mut pos,
        mut game_mode,
        mut inventory,
        mut client,
    ) in &mut clients
    {
        let layer = layers.single();

        layer_id.0 = layer;
        visible_chunk_layer.0 = layer;
        visible_entity_layers.0.insert(layer);
        pos.set([0.5, 65.0, 0.5]);
        *game_mode = GameMode::Survival;

        inventory.set_slot(
            36,
            ItemStack::new(ItemKind::IronSword, 1).with_components(vec![
                ItemComponent::Unbreakable,
                ItemComponent::ItemName(
                    "This sword is unbreakable and does 100 damage".into_text_component(),
                ),
                ItemComponent::Lore(vec!["Very epic".into_text_component()]),
                ItemComponent::AttributeModifiers {
                    modifiers: vec![AttributeModifier {
                        // `attack_damage`, You can find these IDs in `attributes.json`.
                        // The index in this map is the ID of attribute
                        attribute_id: RegistryId::new(2),
                        modifier_id: ident!("my_custom_damage").into(),
                        operation: EntityAttributeOperation::Add,
                        value: 100.0,
                        slot: AttributeSlot::MainHand,
                    }],
                },
            ]),
        );

        inventory.set_slot(
            37,
            ItemStack::new(ItemKind::IronIngot, 99).with_components(vec![
                ItemComponent::MaxStackSize(99.into()),
                ItemComponent::ItemName("This item stacks to 99".into_text_component()),
            ]),
        );

        inventory.set_slot(
            38,
            ItemStack::new(ItemKind::Glass, 1).with_components(vec![
                ItemComponent::ItemName("You can put this on your head".into_text_component()),
                ItemComponent::Equippable {
                    slot: EquipSlot::Head,
                    // The component itself does not actually play the sound,
                    // It just acts as a data marker.
                    equip_sound: IdOr::Inline(SoundEventDefinition {
                        sound: ident!("minecraft:entity.arrow.hit").into(),
                        range: None,
                    }),
                    model: None,
                    camera_overlay: None,
                    allowed_entities: None,
                    dispensable: true,
                    swappable: true,
                    damage_on_hurt: false,
                    shearing_sound: None,
                },
            ]),
        );

        inventory.set_slot(
            39,
            ItemStack::new(ItemKind::LeatherChestplate, 1).with_components(vec![
                ItemComponent::ItemName(
                    "Dyed leather chestplate with enchantments".into_text_component(),
                ),
                ItemComponent::DyedColor { color: 0xff0000 },
                ItemComponent::Enchantments(vec![
                    // Protection IV, You can find these IDs in the entry `minecraft:enchantments`
                    // in `registry_codec.json`. The index in this map is the ID of the
                    // enchantment.
                    (RegistryId::new(27), VarInt(4)),
                ]),
            ]),
        );

        inventory.set_slot(
            40,
            ItemStack::new(ItemKind::Compass, 1).with_components(vec![
                ItemComponent::ItemName("This compass spins randomly".into_text_component()),
                ItemComponent::LodestoneTracker {
                    target: None,   /* `None` makes the compass spin, provide a position so it
                                     * points towards it. */
                    tracked: false, // If the component gets removed if the lodestone is broken.
                },
            ]),
        );

        inventory.set_slot(
            41,
            ItemStack::new(ItemKind::PlayerHead, 1).with_components(vec![ItemComponent::Profile(
                ResolvableProfile {
                    name: Some("Notch".to_owned()),
                    id: None,
                    properties: Vec::new(),
                },
            )]),
        );

        inventory.set_slot(
            42,
            ItemStack::new(ItemKind::Anvil, 1).with_components(vec![
                ItemComponent::ItemName("Eat me!".into_text_component()),
                ItemComponent::Lore(vec!["Rich in iron".into_text_component()]),
                ItemComponent::Consumable {
                    consume_seconds: 3.0,
                    animation: ConsumableAnimation::Eat,
                    sound: IdOr::Inline(SoundEventDefinition {
                        sound: ident!("minecraft:block.anvil.use").into(),
                        range: None,
                    }),
                    has_consume_particles: true,
                    effects: Vec::new(),
                },
            ]),
        );

        let duplicate_bug_marker = "Try this: pick up the Probe A sword, then click Probe B.";
        client.send_chat_message(duplicate_bug_marker);
        client.send_chat_message(
            "If component hashes are ignored, both stacks can become Probe A instead of swapping.",
        );

        inventory.set_slot(
            43,
            ItemStack::new(ItemKind::IronSword, 1).with_components(vec![
                ItemComponent::ItemName("Probe A - red name".into_text_component()),
                ItemComponent::Lore(vec![
                    "Same item/count as Probe B".into_text_component(),
                    "Component payload A".into_text_component(),
                ]),
                ItemComponent::DyedColor { color: 0xff0000 },
            ]),
        );

        inventory.set_slot(
            44,
            ItemStack::new(ItemKind::IronSword, 1).with_components(vec![
                ItemComponent::ItemName("Probe B - blue name".into_text_component()),
                ItemComponent::Lore(vec![
                    "Same item/count as Probe A".into_text_component(),
                    "Component payload B".into_text_component(),
                ]),
                ItemComponent::DyedColor { color: 0x0000ff },
            ]),
        );
    }
}

fn report_probe_clicks(
    mut events: EventReader<ClickSlotEvent>,
    mut clients: Query<(&mut Client, &Inventory, &CursorItem)>,
) {
    for event in events.read() {
        let touched_probe_slot = event.slot_id == 43
            || event.slot_id == 44
            || event
                .slot_changes
                .iter()
                .any(|change| change.idx == 43 || change.idx == 44);

        if !touched_probe_slot {
            continue;
        }

        let Ok((mut client, inventory, cursor)) = clients.get_mut(event.client) else {
            continue;
        };

        client.send_chat_message(format!(
            "Server state: slot 43 = {}, slot 44 = {}, cursor = {}",
            probe_label(inventory.slot(43)),
            probe_label(inventory.slot(44)),
            probe_label(&cursor.0),
        ));
    }
}

fn probe_label(stack: &ItemStack) -> String {
    if stack.is_empty() {
        return "empty".to_owned();
    }

    let name = stack
        .components()
        .into_iter()
        .find_map(|component| match component {
            ItemComponent::ItemName(name) => match &name.as_text().content {
                TextContent::Text { text } => Some(text.as_ref().to_owned()),
                _ => Some(name.as_text().to_string()),
            },
            _ => None,
        })
        .unwrap_or_else(|| format!("{:?}", stack.item));

    format!("{name} x{}", stack.count)
}
