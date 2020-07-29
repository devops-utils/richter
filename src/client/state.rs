use std::{cell::RefCell, collections::HashMap, rc::Rc};

use crate::{
    client::{
        entity::{
            particle::{Particles, TrailKind, MAX_PARTICLES},
            Beam, ClientEntity, LightDesc, Lights, MAX_BEAMS, MAX_LIGHTS, MAX_TEMP_ENTITIES,
        },
        input::game::{Action, GameInput},
        sound::{AudioSource, Listener, StaticSound},
        view::{MouseVars, View},
        ClientError, ColorShiftCode, IntermissionKind, Mixer, MoveVars, MAX_STATS,
    },
    common::{
        bsp, engine,
        model::{Model, ModelFlags, ModelKind, SyncType},
        net::{
            self, BeamEntityKind, ButtonFlags, ColorShift, EntityEffects, ItemFlags,
            PointEntityKind, TempEntity,
        },
        vfs::Vfs,
    },
};
use cgmath::{Angle as _, Deg, InnerSpace as _, Matrix4, Vector3, Zero as _};
use chrono::Duration;
use net::{ClientCmd, EntityState, EntityUpdate, PlayerColor};
use rand::distributions::{Distribution as _, Uniform};

pub struct PlayerInfo {
    pub name: String,
    pub frags: i32,
    pub colors: PlayerColor,
    // translations: [u8; VID_GRADES],
}

// client information regarding the current level
pub struct ClientState {
    // model precache
    pub models: Vec<Model>,
    // name-to-id map
    pub model_names: HashMap<String, usize>,

    // audio source precache
    pub sounds: Vec<AudioSource>,

    // ambient sounds (infinite looping, static position)
    pub static_sounds: Vec<StaticSound>,

    // entities and entity-like things
    pub entities: Vec<ClientEntity>,
    pub static_entities: Vec<ClientEntity>,
    pub temp_entities: Vec<ClientEntity>,
    // dynamic point lights
    pub lights: Lights,
    // lightning bolts and grappling hook cable
    pub beams: [Option<Beam>; MAX_BEAMS],
    // particle effects
    pub particles: Particles,

    // visible entities, rebuilt per-frame
    pub visible_entity_ids: Vec<usize>,

    pub light_styles: HashMap<u8, String>,

    // various values relevant to the player and level (see common::net::ClientStat)
    pub stats: [i32; MAX_STATS],

    pub max_players: usize,
    pub player_info: [Option<PlayerInfo>; net::MAX_CLIENTS],

    // the last two timestamps sent by the server (for lerping)
    pub msg_times: [Duration; 2],
    pub time: Duration,
    pub lerp_factor: f32,

    pub items: ItemFlags,
    pub item_get_time: [Duration; net::MAX_ITEMS],
    pub face_anim_time: Duration,
    pub color_shifts: [Rc<RefCell<ColorShift>>; 4],
    pub view: View,

    pub msg_velocity: [Vector3<f32>; 2],
    pub velocity: Vector3<f32>,

    // paused: bool,
    pub on_ground: bool,
    pub in_water: bool,
    pub intermission: Option<IntermissionKind>,
    pub start_time: Duration,
    pub completion_time: Option<Duration>,

    pub mixer: Mixer,
    pub listener: Listener,
}

impl ClientState {
    // TODO: add parameter for number of player slots and reserve them in entity list
    pub fn new(audio_device: Rc<rodio::Device>) -> Result<ClientState, ClientError> {
        Ok(ClientState {
            models: vec![Model::none()],
            model_names: HashMap::new(),
            sounds: Vec::new(),
            static_sounds: Vec::new(),
            entities: Vec::new(),
            static_entities: Vec::new(),
            temp_entities: Vec::new(),
            lights: Lights::with_capacity(MAX_LIGHTS),
            beams: [None; MAX_BEAMS],
            particles: Particles::with_capacity(MAX_PARTICLES),
            visible_entity_ids: Vec::new(),
            light_styles: HashMap::new(),
            stats: [0; MAX_STATS],
            max_players: 0,
            player_info: Default::default(),
            msg_times: [Duration::zero(), Duration::zero()],
            time: Duration::zero(),
            lerp_factor: 0.0,
            items: ItemFlags::empty(),
            item_get_time: [Duration::zero(); net::MAX_ITEMS],
            color_shifts: [
                Rc::new(RefCell::new(ColorShift {
                    dest_color: [0; 3],
                    percent: 0,
                })),
                Rc::new(RefCell::new(ColorShift {
                    dest_color: [0; 3],
                    percent: 0,
                })),
                Rc::new(RefCell::new(ColorShift {
                    dest_color: [0; 3],
                    percent: 0,
                })),
                Rc::new(RefCell::new(ColorShift {
                    dest_color: [0; 3],
                    percent: 0,
                })),
            ],
            view: View::new(),
            face_anim_time: Duration::zero(),
            msg_velocity: [Vector3::zero(), Vector3::zero()],
            velocity: Vector3::zero(),
            on_ground: false,
            in_water: false,
            intermission: None,
            start_time: Duration::zero(),
            completion_time: None,
            mixer: Mixer::new(audio_device.clone()),
            listener: Listener::new(),
        })
    }

    pub fn from_server_info(
        vfs: &Vfs,
        audio_device: Rc<rodio::Device>,
        max_clients: u8,
        model_precache: Vec<String>,
        sound_precache: Vec<String>,
    ) -> Result<ClientState, ClientError> {
        // TODO: validate submodel names
        let mut models = Vec::with_capacity(model_precache.len());
        models.push(Model::none());
        let mut model_names = HashMap::new();
        for mod_name in model_precache {
            // BSPs can have more than one model
            if mod_name.ends_with(".bsp") {
                let bsp_data = vfs.open(&mod_name)?;
                let (mut brush_models, _) = bsp::load(bsp_data).unwrap();
                for bmodel in brush_models.drain(..) {
                    let id = models.len();
                    let name = bmodel.name().to_owned();
                    models.push(bmodel);
                    model_names.insert(name, id);
                }
            } else if !mod_name.starts_with("*") {
                // model names starting with * are loaded from the world BSP
                debug!("Loading model {}", mod_name);
                let id = models.len();
                models.push(Model::load(vfs, &mod_name)?);
                model_names.insert(mod_name, id);
            }

            // TODO: send keepalive message?
        }

        let mut sounds = vec![AudioSource::load(&vfs, "misc/null.wav")?];
        for ref snd_name in sound_precache {
            debug!("Loading sound {}: {}", sounds.len(), snd_name);
            sounds.push(AudioSource::load(vfs, snd_name)?);
            // TODO: send keepalive message?
        }

        Ok(ClientState {
            models,
            model_names,
            sounds,
            max_players: max_clients as usize,
            ..ClientState::new(audio_device)?
        })
    }

    /// Advance the simulation time by the specified amount.
    ///
    /// This method does not change the state of the world to match the new time value.
    pub fn advance_time(&mut self, frame_time: Duration) {
        self.time = self.time + frame_time;
    }

    /// Update the client state interpolation ratio.
    ///
    /// This calculates the ratio used to interpolate entities between the last
    /// two updates from the server.
    pub fn update_interp_ratio(&mut self, cl_nolerp: f32) {
        if cl_nolerp != 0.0 {
            self.time = self.msg_times[0];
            self.lerp_factor = 1.0;
            return;
        }

        let server_delta = engine::duration_to_f32(match self.msg_times[0] - self.msg_times[1] {
            // if no time has passed between updates, don't lerp anything
            d if d == Duration::zero() => {
                self.time = self.msg_times[0];
                self.lerp_factor = 1.0;
                return;
            }

            d if d > Duration::milliseconds(100) => {
                self.msg_times[1] = self.msg_times[0] - Duration::milliseconds(100);
                Duration::milliseconds(100)
            }

            d if d < Duration::zero() => {
                warn!(
                    "Negative time delta from server!: ({})s",
                    engine::duration_to_f32(d)
                );
                d
            }

            d => d,
        });

        let frame_delta = engine::duration_to_f32(self.time - self.msg_times[1]);

        self.lerp_factor = match frame_delta / server_delta {
            f if f < 0.0 => {
                warn!("Negative lerp factor ({})", f);
                if f < -0.01 {
                    self.time = self.msg_times[1];
                }

                0.0
            }

            f if f > 1.0 => {
                warn!("Lerp factor > 1 ({})", f);
                if f > 1.01 {
                    self.time = self.msg_times[0];
                }

                1.0
            }

            f => f,
        }
    }

    /// Update all entities in the game world.
    ///
    /// This method is responsible for the following:
    /// - Updating entity position
    /// - Despawning entities which did not receive an update in the last server
    ///   message
    /// - Spawning particles on entities with particle effects
    /// - Spawning dynamic lights on entities with lighting effects
    pub fn update_entities(&mut self) -> Result<(), ClientError> {
        lazy_static! {
            static ref MFLASH_DIMLIGHT_DISTRIBUTION: Uniform<f32> = Uniform::new(200.0, 232.0);
            static ref BRIGHTLIGHT_DISTRIBUTION: Uniform<f32> = Uniform::new(400.0, 432.0);
        }

        let lerp_factor = self.lerp_factor;

        self.velocity =
            self.msg_velocity[1] + lerp_factor * (self.msg_velocity[0] - self.msg_velocity[1]);

        // TODO: if we're in demo playback, interpolate the view angles

        let obj_rotate = Deg(100.0 * engine::duration_to_f32(self.time)).normalize();

        // rebuild the list of visible entities
        self.visible_entity_ids.clear();

        // in the extremely unlikely event that there's only a world entity and nothing else, just
        // return
        if self.entities.len() <= 1 {
            return Ok(());
        }

        // NOTE that we start at entity 1 since we don't need to link the world entity
        for (ent_id, ent) in self.entities.iter_mut().enumerate().skip(1) {
            if ent.model_id == 0 {
                // nothing in this entity slot
                // TODO: R_RemoveEfrags
                continue;
            }

            // if we didn't get an update this frame, remove the entity
            if ent.msg_time != self.msg_times[0] {
                ent.model_id = 0;
                continue;
            }

            let prev_origin = ent.origin;

            if ent.force_link {
                trace!("force link on entity {}", ent_id);
                ent.origin = ent.msg_origins[0];
                ent.angles = ent.msg_angles[0];
            } else {
                let origin_delta = ent.msg_origins[0] - ent.msg_origins[1];
                let ent_lerp_factor = if origin_delta.magnitude2() > 10_000.0 {
                    // if the entity moved more than 100 units in one frame,
                    // assume it was teleported and don't lerp anything
                    1.0
                } else {
                    lerp_factor
                };

                ent.origin = ent.msg_origins[1] + ent_lerp_factor * origin_delta;

                // assume that entities will not whip around 180+ degrees in one
                // frame and adjust the delta accordingly. this avoids a bug
                // where small turns between 0 <-> 359 cause the demo camera to
                // face backwards for one frame.
                for i in 0..3 {
                    let mut angle_delta = ent.msg_angles[0][i] - ent.msg_angles[1][i];
                    if angle_delta > Deg(180.0) {
                        angle_delta = Deg(360.0) - angle_delta;
                    } else if angle_delta < Deg(-180.0) {
                        angle_delta = Deg(360.0) + angle_delta;
                    }

                    ent.angles[i] =
                        (ent.msg_angles[1][i] + angle_delta * ent_lerp_factor).normalize();
                }
            }

            let model = &self.models[ent.model_id];
            if model.has_flag(ModelFlags::ROTATE) {
                ent.angles[1] = obj_rotate;
            }

            if ent.effects.contains(EntityEffects::BRIGHT_FIELD) {
                self.particles.create_entity_field(self.time, ent);
            }

            // TODO: cache a SmallRng in Client
            let mut rng = rand::thread_rng();

            // TODO: factor out EntityEffects->LightDesc mapping
            if ent.effects.contains(EntityEffects::MUZZLE_FLASH) {
                // TODO: angle and move origin to muzzle
                ent.light_id = Some(self.lights.insert(
                    self.time,
                    LightDesc {
                        origin: ent.origin + Vector3::new(0.0, 0.0, 16.0),
                        init_radius: MFLASH_DIMLIGHT_DISTRIBUTION.sample(&mut rng),
                        decay_rate: 0.0,
                        min_radius: Some(32.0),
                        ttl: Duration::milliseconds(100),
                    },
                    ent.light_id,
                ));
            }

            if ent.effects.contains(EntityEffects::BRIGHT_LIGHT) {
                ent.light_id = Some(self.lights.insert(
                    self.time,
                    LightDesc {
                        origin: ent.origin,
                        init_radius: BRIGHTLIGHT_DISTRIBUTION.sample(&mut rng),
                        decay_rate: 0.0,
                        min_radius: None,
                        ttl: Duration::milliseconds(1),
                    },
                    ent.light_id,
                ));
            }

            if ent.effects.contains(EntityEffects::DIM_LIGHT) {
                ent.light_id = Some(self.lights.insert(
                    self.time,
                    LightDesc {
                        origin: ent.origin,
                        init_radius: MFLASH_DIMLIGHT_DISTRIBUTION.sample(&mut rng),
                        decay_rate: 0.0,
                        min_radius: None,
                        ttl: Duration::milliseconds(1),
                    },
                    ent.light_id,
                ));
            }

            // check if this entity leaves a trail
            let trail_kind = if model.has_flag(ModelFlags::GIB) {
                Some(TrailKind::Blood)
            } else if model.has_flag(ModelFlags::ZOMGIB) {
                Some(TrailKind::BloodSlight)
            } else if model.has_flag(ModelFlags::TRACER) {
                Some(TrailKind::TracerGreen)
            } else if model.has_flag(ModelFlags::TRACER2) {
                Some(TrailKind::TracerRed)
            } else if model.has_flag(ModelFlags::ROCKET) {
                ent.light_id = Some(self.lights.insert(
                    self.time,
                    LightDesc {
                        origin: ent.origin,
                        init_radius: 200.0,
                        decay_rate: 0.0,
                        min_radius: None,
                        ttl: Duration::milliseconds(10),
                    },
                    ent.light_id,
                ));
                Some(TrailKind::Rocket)
            } else if model.has_flag(ModelFlags::GRENADE) {
                Some(TrailKind::Smoke)
            } else if model.has_flag(ModelFlags::TRACER3) {
                Some(TrailKind::Vore)
            } else {
                None
            };

            // if the entity leaves a trail, generate it
            if let Some(kind) = trail_kind {
                self.particles
                    .create_trail(self.time, prev_origin, ent.origin, kind, false);
            }

            // mark entity for rendering
            self.visible_entity_ids.push(ent_id);

            // enable lerp for next frame
            ent.force_link = false;
        }

        // apply effects to static entities as well
        for ent in self.static_entities.iter_mut() {
            let mut rng = rand::thread_rng();

            if ent.effects.contains(EntityEffects::BRIGHT_LIGHT) {
                debug!("spawn bright light on static entity");
                ent.light_id = Some(self.lights.insert(
                    self.time,
                    LightDesc {
                        origin: ent.origin,
                        init_radius: BRIGHTLIGHT_DISTRIBUTION.sample(&mut rng),
                        decay_rate: 0.0,
                        min_radius: None,
                        ttl: Duration::milliseconds(1),
                    },
                    ent.light_id,
                ));
            }

            if ent.effects.contains(EntityEffects::DIM_LIGHT) {
                debug!("spawn dim light on static entity");
                ent.light_id = Some(self.lights.insert(
                    self.time,
                    LightDesc {
                        origin: ent.origin,
                        init_radius: MFLASH_DIMLIGHT_DISTRIBUTION.sample(&mut rng),
                        decay_rate: 0.0,
                        min_radius: None,
                        ttl: Duration::milliseconds(1),
                    },
                    ent.light_id,
                ));
            }
        }

        Ok(())
    }

    pub fn update_temp_entities(&mut self) -> Result<(), ClientError> {
        lazy_static! {
            static ref ANGLE_DISTRIBUTION: Uniform<f32> = Uniform::new(0.0, 360.0);
        }

        self.temp_entities.clear();
        for id in 0..self.beams.len() {
            // remove beam if expired
            if self.beams[id].map_or(false, |b| b.expire < self.time) {
                self.beams[id] = None;
                continue;
            }

            let view_ent = self.view_entity_id();
            if let Some(ref mut beam) = self.beams[id] {
                // keep lightning gun bolts fixed to player
                if beam.entity_id == view_ent {
                    beam.start = self.entities[view_ent].origin;
                }

                let vec = beam.end - beam.start;
                let yaw = Deg::from(cgmath::Rad(vec.y.atan2(vec.x))).normalize();
                let forward = (vec.x.powf(2.0) + vec.y.powf(2.0)).sqrt();
                let pitch = Deg::from(cgmath::Rad(vec.z.atan2(forward))).normalize();

                let len = vec.magnitude();
                let direction = vec.normalize();
                for interval in 0..(len / 30.0) as i32 {
                    let mut ent = ClientEntity::uninitialized();
                    ent.origin = beam.start + 30.0 * interval as f32 * direction;
                    ent.angles = Vector3::new(
                        pitch,
                        yaw,
                        Deg(ANGLE_DISTRIBUTION.sample(&mut rand::thread_rng())),
                    );

                    if self.temp_entities.len() < MAX_TEMP_ENTITIES {
                        self.temp_entities.push(ent);
                    } else {
                        warn!("too many temp entities!");
                    }
                }
            }
        }

        Ok(())
    }

    pub fn handle_input(
        &mut self,
        game_input: &mut GameInput,
        frame_time: Duration,
        move_vars: MoveVars,
        mouse_vars: MouseVars,
    ) -> ClientCmd {
        use Action::*;

        let mlook = game_input.action_state(MLook);
        self.view.handle_input(
            frame_time,
            game_input,
            self.intermission.as_ref(),
            mlook,
            move_vars.cl_anglespeedkey,
            move_vars.cl_pitchspeed,
            move_vars.cl_yawspeed,
            mouse_vars,
        );

        let mut move_left = game_input.action_state(MoveLeft);
        let mut move_right = game_input.action_state(MoveRight);
        if game_input.action_state(Strafe) {
            move_left |= game_input.action_state(Left);
            move_right |= game_input.action_state(Right);
        }

        let mut sidemove = move_vars.cl_sidespeed * (move_right as i32 - move_left as i32) as f32;

        let mut upmove = move_vars.cl_upspeed
            * (game_input.action_state(MoveUp) as i32 - game_input.action_state(MoveDown) as i32)
                as f32;

        let mut forwardmove = 0.0;
        if !game_input.action_state(KLook) {
            forwardmove +=
                move_vars.cl_forwardspeed * game_input.action_state(Forward) as i32 as f32;
            forwardmove -= move_vars.cl_backspeed * game_input.action_state(Back) as i32 as f32;
        }

        if game_input.action_state(Speed) {
            sidemove *= move_vars.cl_movespeedkey;
            upmove *= move_vars.cl_movespeedkey;
            forwardmove *= move_vars.cl_movespeedkey;
        }

        let mut button_flags = ButtonFlags::empty();

        if game_input.action_state(Attack) {
            button_flags |= ButtonFlags::ATTACK;
        }

        if game_input.action_state(Jump) {
            button_flags |= ButtonFlags::JUMP;
        }

        if !mlook {
            // TODO: IN_Move (mouse / joystick / gamepad)
        }

        let send_time = self.msg_times[0];
        // send "raw" angles without any pitch/roll from movement or damage
        let angles = self.view.input_angles();

        ClientCmd::Move {
            send_time,
            angles: Vector3::new(angles.pitch, angles.yaw, angles.roll),
            fwd_move: forwardmove as i16,
            side_move: sidemove as i16,
            up_move: upmove as i16,
            button_flags,
            impulse: game_input.impulse(),
        }
    }

    /// Spawn an entity with the given ID, also spawning any uninitialized
    /// entities between the former last entity and the new one.
    // TODO: skipping entities indicates that the entities have been freed by
    // the server. it may make more sense to use a HashMap to store entities by
    // ID since the lookup table is relatively sparse.
    pub fn spawn_entities(&mut self, id: usize, baseline: EntityState) -> Result<(), ClientError> {
        // don't clobber existing entities
        if id < self.entities.len() {
            Err(ClientError::EntityExists(id))?;
        }

        // spawn intermediate entities (uninitialized)
        for i in self.entities.len()..id {
            debug!("Spawning uninitialized entity with ID {}", i);
            self.entities.push(ClientEntity::uninitialized());
        }

        debug!(
            "Spawning entity with id {} from baseline {:?}",
            id, baseline
        );
        self.entities.push(ClientEntity::from_baseline(baseline));

        Ok(())
    }

    pub fn update_entity(&mut self, id: usize, update: EntityUpdate) -> Result<(), ClientError> {
        if id > self.entities.len() {
            let baseline = EntityState {
                origin: Vector3::new(
                    update.origin_x.unwrap_or(0.0),
                    update.origin_y.unwrap_or(0.0),
                    update.origin_z.unwrap_or(0.0),
                ),
                angles: Vector3::new(
                    update.pitch.unwrap_or(Deg(0.0)),
                    update.yaw.unwrap_or(Deg(0.0)),
                    update.roll.unwrap_or(Deg(0.0)),
                ),
                model_id: update.model_id.unwrap_or(0) as usize,
                frame_id: update.frame_id.unwrap_or(0) as usize,
                colormap: update.colormap.unwrap_or(0),
                skin_id: update.skin_id.unwrap_or(0) as usize,
                effects: EntityEffects::empty(),
            };

            self.spawn_entities(id, baseline)?;
        }

        let entity = &mut self.entities[id];
        entity.update(self.msg_times, update);
        if entity.model_changed() {
            match self.models[entity.model_id].kind() {
                ModelKind::None => (),
                _ => {
                    entity.sync_base = match self.models[entity.model_id].sync_type() {
                        SyncType::Sync => Duration::zero(),
                        SyncType::Rand => unimplemented!(), // TODO
                    }
                }
            }
        }

        if let Some(_c) = entity.colormap() {
            // only players may have custom colormaps
            if id > self.max_players {
                warn!(
                    "Server attempted to set colormap on entity {}, which is not a player",
                    id
                );
            }
            // TODO: set player custom colormaps
        }

        Ok(())
    }

    pub fn spawn_temp_entity(&mut self, temp_entity: &TempEntity) {
        match temp_entity {
            TempEntity::Point { kind, origin } => {
                use PointEntityKind::*;
                match kind {
                    // projectile impacts
                    WizSpike | KnightSpike | Spike | SuperSpike | Gunshot => {
                        let (color, count) = match kind {
                            // TODO: start wizard/hit.wav
                            WizSpike => (20, 30),

                            // TODO: start hknight/hit.wav
                            KnightSpike => (226, 20),

                            // TODO: for Spike and SuperSpike, start one of:
                            // - 26.67%: weapons/tink1.wav
                            // - 20.0%: weapons/ric1.wav
                            // - 20.0%: weapons/ric2.wav
                            // - 20.0%: weapons/ric3.wav
                            Spike => (0, 10),
                            SuperSpike => (0, 20),

                            // no sound
                            Gunshot => (0, 20),
                            _ => unreachable!(),
                        };

                        self.particles.create_projectile_impact(
                            self.time,
                            *origin,
                            Vector3::zero(),
                            color,
                            count,
                        );
                    }

                    Explosion => {
                        self.particles.create_explosion(self.time, *origin);
                        self.lights.insert(
                            self.time,
                            LightDesc {
                                origin: *origin,
                                init_radius: 350.0,
                                decay_rate: 300.0,
                                min_radius: None,
                                ttl: Duration::milliseconds(500),
                            },
                            None,
                        );
                        // TODO: start weapons/r_exp3
                    }

                    ColorExplosion {
                        color_start,
                        color_len,
                    } => {
                        self.particles.create_color_explosion(
                            self.time,
                            *origin,
                            (*color_start)..=(*color_start + *color_len - 1),
                        );
                        self.lights.insert(
                            self.time,
                            LightDesc {
                                origin: *origin,
                                init_radius: 350.0,
                                decay_rate: 300.0,
                                min_radius: None,
                                ttl: Duration::milliseconds(500),
                            },
                            None,
                        );
                        // TODO: start weapons/r_exp3
                    }

                    TarExplosion => {
                        self.particles.create_spawn_explosion(self.time, *origin);
                        // TODO: start weapons/r_exp3 (same sound as rocket explosion)
                    }

                    LavaSplash => self.particles.create_lava_splash(self.time, *origin),
                    Teleport => self.particles.create_teleporter_warp(self.time, *origin),
                }
            }

            TempEntity::Beam {
                kind,
                entity_id,
                start,
                end,
            } => {
                use BeamEntityKind::*;
                let model_name = match kind {
                    Lightning { model_id } => format!(
                        "progs/bolt{}.mdl",
                        match model_id {
                            1 => "",
                            2 => "2",
                            3 => "3",
                            x => panic!("invalid lightning model id: {}", x),
                        }
                    ),
                    Grapple => "progs/beam.mdl".to_string(),
                };

                self.spawn_beam(
                    self.time,
                    *entity_id as usize,
                    *self.model_names.get(&model_name).unwrap(),
                    *start,
                    *end,
                );
            }
        }
    }

    pub fn spawn_beam(
        &mut self,
        time: Duration,
        entity_id: usize,
        model_id: usize,
        start: Vector3<f32>,
        end: Vector3<f32>,
    ) {
        // always override beam with same entity_id if it exists
        // otherwise use the first free slot
        let mut free = None;
        for i in 0..self.beams.len() {
            if let Some(ref mut beam) = self.beams[i] {
                if beam.entity_id == entity_id {
                    beam.model_id = model_id;
                    beam.expire = time + Duration::milliseconds(200);
                    beam.start = start;
                    beam.end = end;
                }
            } else if free.is_none() {
                free = Some(i);
            }
        }

        if let Some(i) = free {
            self.beams[i] = Some(Beam {
                entity_id,
                model_id,
                expire: time + Duration::milliseconds(200),
                start,
                end,
            });
        } else {
            warn!("No free beam slots!");
        }
    }

    pub fn update_listener(&self) {
        // TODO: update to self.view_origin()
        let view_origin = self.entities[self.view.entity_id()].origin;
        let world_translate = Matrix4::from_translation(view_origin);

        let left_base = Vector3::new(0.0, 4.0, self.view.view_height());
        let right_base = Vector3::new(0.0, -4.0, self.view.view_height());

        let rotate = self.view.input_angles().mat4_quake();

        let left = (world_translate * rotate * left_base.extend(1.0)).truncate();
        let right = (world_translate * rotate * right_base.extend(1.0)).truncate();

        self.listener.set_origin(view_origin);
        self.listener.set_left_ear(left);
        self.listener.set_right_ear(right);
    }

    pub fn update_sound_spatialization(&self) {
        self.update_listener();

        // update entity sounds
        for opt_chan in self.mixer.channels.iter() {
            if let Some(ref chan) = opt_chan {
                if chan.channel.in_use() {
                    chan.channel
                        .update(self.entities[chan.ent_id].origin, &self.listener);
                }
            }
        }

        // update static sounds
        for ss in self.static_sounds.iter() {
            ss.update(&self.listener);
        }
    }

    fn view_leaf_contents(&self) -> Result<bsp::BspLeafContents, ClientError> {
        match self.models[1].kind() {
            ModelKind::Brush(ref bmodel) => {
                let bsp_data = bmodel.bsp_data();
                let leaf_id = bsp_data.find_leaf(self.entities[self.view.entity_id()].origin);
                let leaf = &bsp_data.leaves()[leaf_id];
                Ok(leaf.contents)
            }
            _ => panic!("non-brush worldmodel"),
        }
    }

    pub fn update_color_shifts(&mut self, frame_time: Duration) -> Result<(), ClientError> {
        let float_time = engine::duration_to_f32(frame_time);

        // set color for leaf contents
        self.color_shifts[ColorShiftCode::Contents as usize].replace(
            match self.view_leaf_contents()? {
                bsp::BspLeafContents::Empty => ColorShift {
                    dest_color: [0, 0, 0],
                    percent: 0,
                },
                bsp::BspLeafContents::Lava => ColorShift {
                    dest_color: [255, 80, 0],
                    percent: 150,
                },
                bsp::BspLeafContents::Slime => ColorShift {
                    dest_color: [0, 25, 5],
                    percent: 150,
                },
                _ => ColorShift {
                    dest_color: [130, 80, 50],
                    percent: 128,
                },
            },
        );

        // decay damage and item pickup shifts
        // always decay at least 1 "percent" (actually 1/255)
        // TODO: make percent an actual percent ([0.0, 1.0])
        let mut dmg_shift = self.color_shifts[ColorShiftCode::Damage as usize].borrow_mut();
        dmg_shift.percent -= ((float_time * 150.0) as i32).max(1);
        dmg_shift.percent = dmg_shift.percent.max(0);

        let mut bonus_shift = self.color_shifts[ColorShiftCode::Bonus as usize].borrow_mut();
        bonus_shift.percent -= ((float_time * 100.0) as i32).max(1);
        bonus_shift.percent = bonus_shift.percent.max(0);
        println!("bonus shift percent = {}", bonus_shift.percent);

        // set power-up overlay
        self.color_shifts[ColorShiftCode::Powerup as usize].replace(
            if self.items.contains(ItemFlags::QUAD) {
                ColorShift {
                    dest_color: [0, 0, 255],
                    percent: 30,
                }
            } else if self.items.contains(ItemFlags::SUIT) {
                ColorShift {
                    dest_color: [0, 255, 0],
                    percent: 20,
                }
            } else if self.items.contains(ItemFlags::INVISIBILITY) {
                ColorShift {
                    dest_color: [100, 100, 100],
                    percent: 100,
                }
            } else if self.items.contains(ItemFlags::INVULNERABILITY) {
                ColorShift {
                    dest_color: [255, 255, 0],
                    percent: 30,
                }
            } else {
                ColorShift {
                    dest_color: [0, 0, 0],
                    percent: 0,
                }
            },
        );

        Ok(())
    }

    pub fn iter_visible_entities(&self) -> impl Iterator<Item = &ClientEntity> + Clone {
        self.visible_entity_ids
            .iter()
            .map(move |i| &self.entities[*i])
            .chain(self.temp_entities.iter())
            .chain(self.static_entities.iter())
    }

    pub fn check_entity_id(&self, id: usize) -> Result<(), ClientError> {
        match id {
            0 => Err(ClientError::NullEntity),
            e if e >= self.entities.len() => Err(ClientError::NoSuchEntity(id)),
            _ => Ok(()),
        }
    }

    pub fn check_player_id(&self, id: usize) -> Result<(), ClientError> {
        if id >= net::MAX_CLIENTS {
            Err(ClientError::NoSuchClient(id))
        } else if id > self.max_players {
            Err(ClientError::NoSuchPlayer(id))
        } else {
            Ok(())
        }
    }

    pub fn view_entity_id(&self) -> usize {
        self.view.entity_id()
    }
}
