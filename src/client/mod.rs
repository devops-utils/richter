// Copyright © 2020 Cormac O'Brien
//
// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files (the "Software"), to deal
// in the Software without restriction, including without limitation the rights
// to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
// copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in
// all copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
// OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
// SOFTWARE.

mod cvars;
mod demo;
pub mod entity;
pub mod input;
pub mod menu;
pub mod render;
pub mod sound;
pub mod state;
pub mod trace;
pub mod view;

pub use self::cvars::register_cvars;

use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    io::BufReader,
    net::ToSocketAddrs,
    rc::Rc,
};

use crate::{
    client::{
        demo::{DemoServer, DemoServerError},
        entity::{particle::Particle, ClientEntity, Light, MAX_STATIC_ENTITIES},
        input::game::GameInput,
        sound::{AudioSource, Channel, Listener, StaticSound},
        state::{ClientState, PlayerInfo},
        trace::{TraceEntity, TraceFrame},
        view::{IdleVars, KickVars, MouseVars, RollVars},
    },
    common::{
        console::{CmdRegistry, Console, ConsoleError, CvarRegistry},
        engine,
        math::Angles,
        model::{Model, ModelError},
        net::{
            self,
            connect::{ConnectSocket, Request, Response, CONNECT_PROTOCOL_VERSION},
            BlockingMode, ClientCmd, ClientStat, ColorShift, EntityEffects, EntityState, GameType,
            ItemFlags, NetError, PlayerColor, QSocket, ServerCmd, SignOnStage,
        },
        vfs::{Vfs, VfsError},
    },
};

use cgmath::{Deg, Vector3};
use chrono::Duration;
use sound::SoundError;
use thiserror::Error;

// connections are tried 3 times, see
// https://github.com/id-Software/Quake/blob/master/WinQuake/net_dgrm.c#L1248
const MAX_CONNECT_ATTEMPTS: usize = 3;
const MAX_STATS: usize = 32;

const DEFAULT_SOUND_PACKET_VOLUME: u8 = 255;
const DEFAULT_SOUND_PACKET_ATTENUATION: f32 = 1.0;

const MAX_CHANNELS: usize = 128;

#[derive(Error, Debug)]
pub enum ClientError {
    #[error("Connection rejected: {0}")]
    ConnectionRejected(String),
    #[error("Couldn't read cvar value: {0}")]
    Cvar(ConsoleError),
    #[error("Server sent an invalid port number ({0})")]
    InvalidConnectPort(i32),
    #[error("Server sent an invalid connect response")]
    InvalidConnectResponse,
    #[error("Invalid server address")]
    InvalidServerAddress,
    #[error("No response from server")]
    NoResponse,
    #[error("Unrecognized protocol: {0}")]
    UnrecognizedProtocol(i32),
    #[error("Client is not connected")]
    NotConnected,
    #[error("No client with ID {0}")]
    NoSuchClient(usize),
    #[error("No player with ID {0}")]
    NoSuchPlayer(usize),
    #[error("No entity with ID {0}")]
    NoSuchEntity(usize),
    #[error("Null entity access")]
    NullEntity,
    #[error("Entity already exists: {0}")]
    EntityExists(usize),
    #[error("Invalid view entity: {0}")]
    InvalidViewEntity(usize),
    #[error("Too many static entities")]
    TooManyStaticEntities,
    #[error("No such lightmap animation: {0}")]
    NoSuchLightmapAnimation(usize),
    #[error("Demo server error: {0}")]
    DemoServer(#[from] DemoServerError),
    #[error("Model error: {0}")]
    Model(#[from] ModelError),
    #[error("Network error: {0}")]
    Network(#[from] NetError),
    #[error("Failed to load sound: {0}")]
    Sound(#[from] SoundError),
    #[error("Virtual filesystem error: {0}")]
    Vfs(#[from] VfsError),
}

pub struct MoveVars {
    cl_anglespeedkey: f32,
    cl_pitchspeed: f32,
    cl_yawspeed: f32,
    cl_sidespeed: f32,
    cl_upspeed: f32,
    cl_forwardspeed: f32,
    cl_backspeed: f32,
    cl_movespeedkey: f32,
}

#[derive(Debug, FromPrimitive)]
enum ColorShiftCode {
    Contents = 0,
    Damage = 1,
    Bonus = 2,
    Powerup = 3,
}

struct ServerInfo {
    _max_clients: u8,
    _game_type: GameType,
}

#[derive(Clone, Debug)]
pub enum IntermissionKind {
    Intermission,
    Finale { text: String },
    Cutscene { text: String },
}

struct ClientChannel {
    start_time: Duration,
    ent_id: usize,
    ent_channel: i8,
    channel: Channel,
}

pub struct Mixer {
    audio_device: Rc<rodio::Device>,
    // TODO: replace with an array once const type parameters are implemented
    channels: Box<[Option<ClientChannel>]>,
}

impl Mixer {
    pub fn new(audio_device: Rc<rodio::Device>) -> Mixer {
        let mut channel_vec = Vec::new();

        for _ in 0..MAX_CHANNELS {
            channel_vec.push(None);
        }

        Mixer {
            audio_device,
            channels: channel_vec.into_boxed_slice(),
        }
    }

    fn find_free_channel(&self, ent_id: usize, ent_channel: i8) -> usize {
        let mut oldest = 0;

        for (i, channel) in self.channels.iter().enumerate() {
            match *channel {
                Some(ref chan) => {
                    // if this channel is free, return it right away
                    if !chan.channel.in_use() {
                        return i;
                    }

                    // replace sounds on the same entity channel
                    if ent_channel != 0
                        && chan.ent_id == ent_id
                        && (chan.ent_channel == ent_channel || ent_channel == -1)
                    {
                        return i;
                    }

                    // TODO: don't clobber player sounds with monster sounds

                    // keep track of which sound started the earliest
                    match self.channels[oldest] {
                        Some(ref o) => {
                            if chan.start_time < o.start_time {
                                oldest = i;
                            }
                        }
                        None => oldest = i,
                    }
                }

                None => return i,
            }
        }

        // if there are no good channels, just replace the one that's been running the longest
        oldest
    }

    pub fn start_sound(
        &mut self,
        src: AudioSource,
        time: Duration,
        ent_id: usize,
        ent_channel: i8,
        volume: f32,
        attenuation: f32,
        ents: &[ClientEntity],
        listener: &Listener,
    ) {
        let chan_id = self.find_free_channel(ent_id, ent_channel);
        let new_channel = Channel::new(self.audio_device.clone());

        new_channel.play(
            src.clone(),
            ents[ent_id].origin,
            listener,
            volume,
            attenuation,
        );
        self.channels[chan_id] = Some(ClientChannel {
            start_time: time,
            ent_id,
            ent_channel,
            channel: new_channel,
        })
    }
}

enum ConnectionKind {
    Server { qsock: QSocket, compose: Vec<u8> },
    Demo(DemoServer),
}

struct Connection {
    signon: Rc<Cell<SignOnStage>>,
    state: ClientState,
    kind: ConnectionKind,
}

enum ConnectionStatus {
    Maintain,
    Disconnect,
}

impl Connection {
    fn handle_signon(&mut self, stage: SignOnStage) -> Result<(), ClientError> {
        if let ConnectionKind::Server {
            ref mut compose, ..
        } = self.kind
        {
            match stage {
                SignOnStage::Not => (), // TODO this is an error (invalid value)
                SignOnStage::Prespawn => {
                    ClientCmd::StringCmd {
                        cmd: String::from("prespawn"),
                    }
                    .serialize(compose)?;
                }
                SignOnStage::ClientInfo => {
                    // TODO: fill in client info here
                    ClientCmd::StringCmd {
                        cmd: format!("name \"{}\"\n", "UNNAMED"),
                    }
                    .serialize(compose)?;
                    ClientCmd::StringCmd {
                        cmd: format!("color {} {}", 0, 0),
                    }
                    .serialize(compose)?;
                    // TODO: need default spawn parameters?
                    ClientCmd::StringCmd {
                        cmd: format!("spawn {}", ""),
                    }
                    .serialize(compose)?;
                }
                SignOnStage::Begin => {
                    ClientCmd::StringCmd {
                        cmd: String::from("begin"),
                    }
                    .serialize(compose)?;
                }
                SignOnStage::Done => {
                    debug!("SignOn complete");
                    // TODO: end load screen
                    self.state.start_time = self.state.time;
                }
            }
        }

        self.signon.set(stage);

        Ok(())
    }

    fn parse_server_msg(
        &mut self,
        vfs: &Vfs,
        cmds: &mut CmdRegistry,
        console: &mut Console,
        audio_device: &rodio::Device,
        kick_vars: KickVars,
    ) -> Result<ConnectionStatus, ClientError> {
        use ConnectionStatus::*;

        let (msg, demo_view_angles) = match self.kind {
            ConnectionKind::Server { ref mut qsock, .. } => {
                let msg = qsock.recv_msg(match self.signon.get() {
                    // if we're in the game, don't block waiting for messages
                    SignOnStage::Done => BlockingMode::NonBlocking,

                    // otherwise, give the server some time to respond
                    // TODO: might make sense to make this a future or something
                    _ => BlockingMode::Timeout(Duration::seconds(5)),
                })?;

                (msg, None)
            }

            ConnectionKind::Demo(ref mut demo_srv) => {
                // only get the next update once we've made it all the way to
                // the previous one
                if self.state.time >= self.state.msg_times[0] {
                    let msg_view = match demo_srv.next() {
                        Some(v) => v,
                        None => {
                            return Ok(Disconnect);
                        }
                    };

                    let mut view_angles = msg_view.view_angles();
                    // invert entity angles to get the camera direction right.
                    // yaw is already inverted.
                    view_angles.x = -view_angles.x;
                    view_angles.z = -view_angles.z;

                    // TODO: we shouldn't have to copy the message here
                    (msg_view.message().to_owned(), Some(view_angles))
                } else {
                    (Vec::new(), None)
                }
            }
        };

        // no data available at this time
        if msg.is_empty() {
            return Ok(Maintain);
        }

        let mut reader = BufReader::new(msg.as_slice());

        while let Some(cmd) = ServerCmd::deserialize(&mut reader)? {
            match cmd {
                // TODO: have an error for this instead of panicking
                // once all other commands have placeholder handlers, just error
                // in the wildcard branch
                ServerCmd::Bad => panic!("Invalid command from server"),

                ServerCmd::NoOp => (),

                ServerCmd::CdTrack { .. } => {
                    // TODO: play CD track
                    warn!("CD tracks not yet implemented");
                }

                ServerCmd::CenterPrint { text } => {
                    // TODO: print to center of screen
                    warn!("Center print not yet implemented!");
                    println!("{}", text);
                }

                ServerCmd::ClientData {
                    view_height,
                    ideal_pitch,
                    punch_pitch,
                    velocity_x,
                    punch_yaw,
                    velocity_y,
                    punch_roll,
                    velocity_z,
                    items,
                    on_ground,
                    in_water,
                    weapon_frame,
                    armor,
                    weapon,
                    health,
                    ammo,
                    ammo_shells,
                    ammo_nails,
                    ammo_rockets,
                    ammo_cells,
                    active_weapon,
                } => {
                    self.state
                        .view
                        .set_view_height(view_height.unwrap_or(net::DEFAULT_VIEWHEIGHT));
                    self.state
                        .view
                        .set_ideal_pitch(ideal_pitch.unwrap_or(Deg(0.0)));
                    self.state.view.set_punch_angles(Angles {
                        pitch: punch_pitch.unwrap_or(Deg(0.0)),
                        roll: punch_roll.unwrap_or(Deg(0.0)),
                        yaw: punch_yaw.unwrap_or(Deg(0.0)),
                    });

                    // store old velocity
                    self.state.msg_velocity[1] = self.state.msg_velocity[0];
                    self.state.msg_velocity[0].x = velocity_x.unwrap_or(0.0);
                    self.state.msg_velocity[0].y = velocity_y.unwrap_or(0.0);
                    self.state.msg_velocity[0].z = velocity_z.unwrap_or(0.0);

                    let item_diff = items - self.state.items;
                    if !item_diff.is_empty() {
                        // item flags have changed, something got picked up
                        let bits = item_diff.bits();
                        for i in 0..net::MAX_ITEMS {
                            if bits & 1 << i != 0 {
                                // item with flag value `i` was picked up
                                self.state.item_get_time[i] = self.state.time;
                            }
                        }
                    }
                    self.state.items = items;

                    self.state.on_ground = on_ground;
                    self.state.in_water = in_water;

                    self.state.stats[ClientStat::WeaponFrame as usize] =
                        weapon_frame.unwrap_or(0) as i32;
                    self.state.stats[ClientStat::Armor as usize] = armor.unwrap_or(0) as i32;
                    self.state.stats[ClientStat::Weapon as usize] = weapon.unwrap_or(0) as i32;
                    self.state.stats[ClientStat::Health as usize] = health as i32;
                    self.state.stats[ClientStat::Ammo as usize] = ammo as i32;
                    self.state.stats[ClientStat::Shells as usize] = ammo_shells as i32;
                    self.state.stats[ClientStat::Nails as usize] = ammo_nails as i32;
                    self.state.stats[ClientStat::Rockets as usize] = ammo_rockets as i32;
                    self.state.stats[ClientStat::Cells as usize] = ammo_cells as i32;

                    // TODO: this behavior assumes the `standard_quake` behavior and will likely
                    // break with the mission packs
                    self.state.stats[ClientStat::ActiveWeapon as usize] = active_weapon as i32;
                }

                ServerCmd::Cutscene { text } => {
                    self.state.intermission = Some(IntermissionKind::Cutscene { text });
                    self.state.completion_time = Some(self.state.time);
                }

                ServerCmd::Damage {
                    armor,
                    blood,
                    source,
                } => {
                    self.state.face_anim_time = self.state.time + Duration::milliseconds(200);

                    let dmg_factor = (armor + blood).min(20) as f32 / 2.0;
                    let mut cshift =
                        self.state.color_shifts[ColorShiftCode::Damage as usize].borrow_mut();
                    cshift.percent += 3 * dmg_factor as i32;
                    cshift.percent = cshift.percent.clamp(0, 150);

                    if armor > blood {
                        cshift.dest_color = [200, 100, 100];
                    } else if armor > 0 {
                        cshift.dest_color = [220, 50, 50];
                    } else {
                        cshift.dest_color = [255, 0, 0];
                    }

                    let v_ent = &self.state.entities[self.state.view.entity_id()];

                    let v_angles = Angles {
                        pitch: v_ent.angles.x,
                        roll: v_ent.angles.z,
                        yaw: v_ent.angles.y,
                    };

                    self.state.view.handle_damage(
                        self.state.time,
                        armor as f32,
                        blood as f32,
                        v_ent.origin,
                        v_angles,
                        source,
                        kick_vars,
                    );
                }

                ServerCmd::Disconnect => return Ok(Disconnect),

                ServerCmd::FastUpdate(ent_update) => {
                    // first update signals the last sign-on stage
                    if self.signon.get() == SignOnStage::Begin {
                        self.signon.set(SignOnStage::Done);
                        self.handle_signon(self.signon.get())?;
                    }

                    let ent_id = ent_update.ent_id as usize;
                    self.state.update_entity(ent_id, ent_update)?;

                    // patch view angles in demos
                    if let Some(angles) = demo_view_angles {
                        if ent_id == self.state.view_entity_id() {
                            self.state.entities[ent_id].msg_angles[0] = angles;
                        }
                    }
                }

                ServerCmd::Finale { text } => {
                    self.state.intermission = Some(IntermissionKind::Finale { text });
                    self.state.completion_time = Some(self.state.time);
                }

                ServerCmd::FoundSecret => self.state.stats[ClientStat::FoundSecrets as usize] += 1,
                ServerCmd::Intermission => {
                    self.state.intermission = Some(IntermissionKind::Intermission);
                    self.state.completion_time = Some(self.state.time);
                }
                ServerCmd::KilledMonster => {
                    self.state.stats[ClientStat::KilledMonsters as usize] += 1
                }

                ServerCmd::LightStyle { id, value } => {
                    trace!("Inserting light style {} with value {}", id, &value);
                    let _ = self.state.light_styles.insert(id, value);
                }

                ServerCmd::Particle {
                    origin,
                    direction,
                    count,
                    color,
                } => {
                    match count {
                        // if count is 255, this is an explosion
                        255 => self
                            .state
                            .particles
                            .create_explosion(self.state.time, origin),

                        // otherwise it's an impact
                        _ => self.state.particles.create_projectile_impact(
                            self.state.time,
                            origin,
                            direction,
                            color,
                            count as usize,
                        ),
                    }
                }

                ServerCmd::Print { text } => {
                    // TODO: print to in-game console
                    println!("{}", text);
                }

                ServerCmd::ServerInfo {
                    protocol_version,
                    max_clients,
                    game_type,
                    message,
                    model_precache,
                    sound_precache,
                } => {
                    // check protocol version
                    if protocol_version != net::PROTOCOL_VERSION as i32 {
                        Err(ClientError::UnrecognizedProtocol(protocol_version))?;
                    }

                    // TODO: print sign-on message to in-game console
                    println!("{}", message);

                    let _server_info = ServerInfo {
                        _max_clients: max_clients,
                        _game_type: game_type,
                    };

                    let audio_device = self.state.mixer.audio_device.clone();
                    self.state = ClientState::from_server_info(
                        vfs,
                        audio_device,
                        max_clients,
                        model_precache,
                        sound_precache,
                    )?;

                    // TODO: replace console commands holding `Rc`s to the old ClientState
                    let bonus_cshift =
                        self.state.color_shifts[ColorShiftCode::Bonus as usize].clone();
                    cmds.insert_or_replace(
                        "bf",
                        Box::new(move |_| {
                            bonus_cshift.replace(ColorShift {
                                dest_color: [215, 186, 69],
                                percent: 50,
                            });
                        }),
                    );
                }

                ServerCmd::SetAngle { angles } => {
                    debug!("Set view angles to {:?}", angles);
                    let view_ent = self.state.view_entity_id();
                    self.state.entities[view_ent].set_angles(angles);
                    self.state.view.update_input_angles(Angles {
                        pitch: angles.x,
                        roll: angles.z,
                        yaw: angles.y,
                    });
                }

                ServerCmd::SetView { ent_id } => {
                    // view entity may not have been spawned yet, so check
                    // against both max_players and the current number of
                    // entities
                    if ent_id <= 0
                        || (ent_id as usize > self.state.max_players
                            && ent_id as usize >= self.state.entities.len())
                    {
                        Err(ClientError::InvalidViewEntity(ent_id as usize))?;
                    }

                    let ent_id = ent_id as usize;

                    debug!("Set view entity to {}", ent_id);
                    self.state.view.set_entity_id(ent_id);
                }

                ServerCmd::SignOnStage { stage } => self.handle_signon(stage)?,

                ServerCmd::Sound {
                    volume,
                    attenuation,
                    entity_id,
                    channel,
                    sound_id,
                    position: _,
                } => {
                    trace!(
                        "starting sound with id {} on entity {} channel {}",
                        sound_id,
                        entity_id,
                        channel
                    );

                    if entity_id as usize >= self.state.entities.len() {
                        warn!(
                            "server tried to start sound on nonexistent entity {}",
                            entity_id
                        );
                        break;
                    }

                    let volume = volume.unwrap_or(DEFAULT_SOUND_PACKET_VOLUME);
                    let attenuation = attenuation.unwrap_or(DEFAULT_SOUND_PACKET_ATTENUATION);
                    // TODO: apply volume, attenuation, spatialization
                    self.state.mixer.start_sound(
                        self.state.sounds[sound_id as usize].clone(),
                        self.state.msg_times[0],
                        entity_id as usize,
                        channel,
                        volume as f32 / 255.0,
                        attenuation,
                        &self.state.entities,
                        &self.state.listener,
                    );
                }

                ServerCmd::SpawnBaseline {
                    ent_id,
                    model_id,
                    frame_id,
                    colormap,
                    skin_id,
                    origin,
                    angles,
                } => {
                    self.state.spawn_entities(
                        ent_id as usize,
                        EntityState {
                            model_id: model_id as usize,
                            frame_id: frame_id as usize,
                            colormap,
                            skin_id: skin_id as usize,
                            origin,
                            angles,
                            effects: EntityEffects::empty(),
                        },
                    )?;
                }

                ServerCmd::SpawnStatic {
                    model_id,
                    frame_id,
                    colormap,
                    skin_id,
                    origin,
                    angles,
                } => {
                    if self.state.static_entities.len() >= MAX_STATIC_ENTITIES {
                        Err(ClientError::TooManyStaticEntities)?;
                    }
                    self.state
                        .static_entities
                        .push(ClientEntity::from_baseline(EntityState {
                            origin,
                            angles,
                            model_id: model_id as usize,
                            frame_id: frame_id as usize,
                            colormap,
                            skin_id: skin_id as usize,
                            effects: EntityEffects::empty(),
                        }));
                }

                ServerCmd::SpawnStaticSound {
                    origin,
                    sound_id,
                    volume,
                    attenuation,
                } => {
                    self.state.static_sounds.push(StaticSound::new(
                        audio_device,
                        origin,
                        self.state.sounds[sound_id as usize].clone(),
                        volume as f32 / 255.0,
                        attenuation as f32 / 64.0,
                        &self.state.listener,
                    ));
                }

                ServerCmd::TempEntity { temp_entity } => self.state.spawn_temp_entity(&temp_entity),

                ServerCmd::StuffText { text } => console.stuff_text(text),

                ServerCmd::Time { time } => {
                    self.state.msg_times[1] = self.state.msg_times[0];
                    self.state.msg_times[0] = engine::duration_from_f32(time);
                }

                ServerCmd::UpdateColors {
                    player_id,
                    new_colors,
                } => {
                    let player_id = player_id as usize;
                    self.state.check_player_id(player_id)?;

                    match self.state.player_info[player_id] {
                        Some(ref mut info) => {
                            trace!(
                                "Player {} (ID {}) colors: {:?} -> {:?}",
                                info.name,
                                player_id,
                                info.colors,
                                new_colors,
                            );
                            info.colors = new_colors;
                        }

                        None => {
                            error!(
                                "Attempted to set colors on nonexistent player with ID {}",
                                player_id
                            );
                        }
                    }
                }

                ServerCmd::UpdateFrags {
                    player_id,
                    new_frags,
                } => {
                    let player_id = player_id as usize;
                    self.state.check_player_id(player_id)?;

                    match self.state.player_info[player_id] {
                        Some(ref mut info) => {
                            trace!(
                                "Player {} (ID {}) frags: {} -> {}",
                                &info.name,
                                player_id,
                                info.frags,
                                new_frags
                            );
                            info.frags = new_frags as i32;
                        }
                        None => {
                            error!(
                                "Attempted to set frags on nonexistent player with ID {}",
                                player_id
                            );
                        }
                    }
                }

                ServerCmd::UpdateName {
                    player_id,
                    new_name,
                } => {
                    let player_id = player_id as usize;
                    self.state.check_player_id(player_id)?;

                    if let Some(ref mut info) = self.state.player_info[player_id] {
                        // if this player is already connected, it's a name change
                        debug!("Player {} has changed name to {}", &info.name, &new_name);
                        info.name = new_name.to_owned();
                    } else {
                        // if this player is not connected, it's a join
                        debug!("Player {} with ID {} has joined", &new_name, player_id);
                        self.state.player_info[player_id] = Some(PlayerInfo {
                            name: new_name.to_owned(),
                            colors: PlayerColor::new(0, 0),
                            frags: 0,
                        });
                    }
                }

                ServerCmd::UpdateStat { stat, value } => {
                    trace!(
                        "{:?}: {} -> {}",
                        stat,
                        self.state.stats[stat as usize],
                        value
                    );
                    self.state.stats[stat as usize] = value;
                }

                ServerCmd::Version { version } => {
                    if version != net::PROTOCOL_VERSION as i32 {
                        // TODO: handle with an error
                        error!(
                            "Incompatible server version: server's is {}, client's is {}",
                            version,
                            net::PROTOCOL_VERSION,
                        );
                        panic!("bad version number");
                    }
                }

                x => {
                    debug!("{:?}", x);
                    unimplemented!();
                }
            }
        }

        Ok(Maintain)
    }

    fn frame(
        &mut self,
        frame_time: Duration,
        vfs: &Vfs,
        cmds: &mut CmdRegistry,
        console: &mut Console,
        audio_device: &rodio::Device,
        kick_vars: KickVars,
        cl_nolerp: f32,
        sv_gravity: f32,
    ) -> Result<(), ClientError> {
        debug!("frame time: {}ms", frame_time.num_milliseconds());

        // do this _before_ parsing server messages so that we know when to
        // request the next message from the demo server.
        self.state.advance_time(frame_time);
        self.parse_server_msg(vfs, cmds, console, audio_device, kick_vars)?;
        self.state.update_interp_ratio(cl_nolerp);

        // interpolate entity data and spawn particle effects, lights
        self.state.update_entities()?;

        // update temp entities (lightning, etc.)
        self.state.update_temp_entities()?;

        // remove expired lights
        self.state.lights.update(self.state.time);

        // apply particle physics and remove expired particles
        self.state
            .particles
            .update(self.state.time, frame_time, sv_gravity);

        if let ConnectionKind::Server {
            ref mut qsock,
            ref mut compose,
        } = self.kind
        {
            // respond to the server
            if qsock.can_send() && !compose.is_empty() {
                qsock.begin_send_msg(&compose)?;
                compose.clear();
            }
        }

        // these all require the player entity to have spawned
        if self.signon.get() == SignOnStage::Done {
            // update ear positions
            self.state.update_listener();

            // spatialize sounds for new ear positions
            self.state.update_sound_spatialization();

            // update camera color shifts for new position/effects
            self.state.update_color_shifts(frame_time)?;
        }

        Ok(())
    }
}

pub struct Client {
    vfs: Rc<Vfs>,
    cvars: Rc<RefCell<CvarRegistry>>,
    cmds: Rc<RefCell<CmdRegistry>>,
    console: Rc<RefCell<Console>>,
    audio_device: Rc<rodio::Device>,
    conn: Option<Connection>,
}

impl Client {
    /// Implements the `reconnect` command.
    fn cmd_reconnect(signon: Rc<Cell<SignOnStage>>) -> Box<dyn Fn(&[&str])> {
        Box::new(move |_| signon.set(SignOnStage::Not))
    }

    pub fn play_demo<S>(
        demo_path: S,
        vfs: Rc<Vfs>,
        cvars: Rc<RefCell<CvarRegistry>>,
        cmds: Rc<RefCell<CmdRegistry>>,
        console: Rc<RefCell<Console>>,
        audio_device: Rc<rodio::Device>,
    ) -> Result<Client, ClientError>
    where
        S: AsRef<str>,
    {
        let mut demo_file = vfs.open(demo_path)?;
        let demo_server = DemoServer::new(&mut demo_file)?;
        let signon = Rc::new(Cell::new(SignOnStage::Not));

        let conn = Some(Connection {
            signon,
            state: ClientState::new(audio_device.clone())?,
            kind: ConnectionKind::Demo(demo_server),
        });

        Ok(Client {
            vfs: vfs.clone(),
            cvars,
            cmds,
            console,
            audio_device: audio_device.clone(),
            conn,
        })
    }

    pub fn connect<A>(
        server_addrs: A,
        vfs: Rc<Vfs>,
        cvars: Rc<RefCell<CvarRegistry>>,
        cmds: Rc<RefCell<CmdRegistry>>,
        console: Rc<RefCell<Console>>,
        audio_device: Rc<rodio::Device>,
    ) -> Result<Client, ClientError>
    where
        A: ToSocketAddrs,
    {
        // set up reconnect
        let signon = Rc::new(Cell::new(SignOnStage::Not));
        cmds.borrow_mut()
            .insert_or_replace("reconnect", Client::cmd_reconnect(signon.clone()));

        let mut con_sock = ConnectSocket::bind("0.0.0.0:0")?;
        let server_addr = match server_addrs.to_socket_addrs() {
            Ok(ref mut a) => a.next().ok_or(ClientError::InvalidServerAddress),
            Err(_) => Err(ClientError::InvalidServerAddress),
        }?;

        let mut response = None;

        for attempt in 0..MAX_CONNECT_ATTEMPTS {
            println!(
                "Connecting...(attempt {} of {})",
                attempt + 1,
                MAX_CONNECT_ATTEMPTS
            );
            con_sock.send_request(
                Request::connect(net::GAME_NAME, CONNECT_PROTOCOL_VERSION),
                server_addr,
            )?;

            // TODO: get rid of magic constant (2.5 seconds wait time for response)
            match con_sock.recv_response(Some(Duration::milliseconds(2500))) {
                Err(err) => {
                    match err {
                        // if the message is invalid, log it but don't quit
                        // TODO: this should probably disconnect
                        NetError::InvalidData(msg) => error!("{}", msg),

                        // other errors are fatal
                        e => return Err(e.into()),
                    }
                }

                Ok(opt) => {
                    if let Some((resp, remote)) = opt {
                        // if this response came from the right server, we're done
                        if remote == server_addr {
                            response = Some(resp);
                            break;
                        }
                    }
                }
            }
        }

        let port = match response.ok_or(ClientError::NoResponse)? {
            Response::Accept(accept) => {
                // validate port number
                if accept.port < 0 || accept.port >= std::u16::MAX as i32 {
                    Err(ClientError::InvalidConnectPort(accept.port))?;
                }

                debug!("Connection accepted on port {}", accept.port);
                accept.port as u16
            }

            // our request was rejected.
            Response::Reject(reject) => Err(ClientError::ConnectionRejected(reject.message))?,

            // the server sent back a response that doesn't make sense here (i.e. something other
            // than an Accept or Reject).
            _ => Err(ClientError::InvalidConnectResponse)?,
        };

        let mut new_addr = server_addr;
        new_addr.set_port(port);

        // we're done with the connection socket, so turn it into a QSocket with the new address
        let qsock = con_sock.into_qsocket(new_addr);

        let conn = Some(Connection {
            signon,
            state: ClientState::new(audio_device.clone())?,
            kind: ConnectionKind::Server {
                qsock,
                compose: Vec::new(),
            },
        });

        Ok(Client {
            vfs: vfs.clone(),
            cvars,
            cmds,
            console,
            audio_device: audio_device.clone(),
            conn,
        })
    }

    pub fn disconnect(&self) {
        unimplemented!();
    }

    fn cvar_value<S>(&self, name: S) -> Result<f32, ClientError>
    where
        S: AsRef<str>,
    {
        self.cvars
            .borrow()
            .get_value(name.as_ref())
            .map_err(ClientError::Cvar)
    }

    pub fn handle_input(
        &mut self,
        game_input: &mut GameInput,
        frame_time: Duration,
    ) -> Result<(), ClientError> {
        let move_vars = self.move_vars()?;
        let mouse_vars = self.mouse_vars()?;

        match self.conn {
            Some(Connection {
                ref mut state,
                kind: ConnectionKind::Server { ref mut qsock, .. },
                ..
            }) => {
                let move_cmd = state.handle_input(game_input, frame_time, move_vars, mouse_vars);
                // TODO: arrayvec here
                let mut msg = Vec::new();
                move_cmd.serialize(&mut msg)?;
                qsock.send_msg_unreliable(&msg)?;

                // clear mouse and impulse
                game_input.refresh();
            }

            _ => (),
        }

        Ok(())
    }

    pub fn get_entity(&self, id: usize) -> Result<&ClientEntity, ClientError> {
        match self.conn {
            Some(Connection { ref state, .. }) => {
                state.check_entity_id(id)?;
                Ok(&state.entities[id])
            }

            None => Err(ClientError::NotConnected),
        }
    }

    pub fn get_entity_mut(&mut self, id: usize) -> Result<&mut ClientEntity, ClientError> {
        match self.conn {
            Some(Connection { ref mut state, .. }) => {
                state.check_entity_id(id)?;
                Ok(&mut state.entities[id])
            }

            None => Err(ClientError::NotConnected),
        }
    }

    pub fn signon_stage(&self) -> Result<SignOnStage, ClientError> {
        match self.conn {
            Some(Connection { ref signon, .. }) => Ok(signon.get()),
            None => Err(ClientError::NotConnected),
        }
    }

    pub fn entities(&self) -> Option<&[ClientEntity]> {
        match self.conn {
            Some(Connection {
                ref signon,
                ref state,
                ..
            }) => match signon.get() {
                SignOnStage::Done => Some(&state.entities),
                _ => None,
            },
            None => None,
        }
    }

    pub fn models(&self) -> Option<&[Model]> {
        match self.conn {
            Some(Connection {
                ref signon,
                ref state,
                ..
            }) => match signon.get() {
                SignOnStage::Done => Some(&state.models),
                _ => None,
            },

            None => None,
        }
    }

    pub fn view_origin(&self) -> Result<Vector3<f32>, ClientError> {
        match self.conn {
            Some(Connection { ref state, .. }) => Ok(state.entities[state.view.entity_id()].origin
                + Vector3::new(0.0, 0.0, state.view.view_height())),

            None => Err(ClientError::NotConnected),
        }
    }

    pub fn view_angles(&self, time: Duration) -> Result<Angles, ClientError> {
        let angles = match self.conn {
            Some(Connection {
                ref state,
                ref kind,
                ..
            }) => match kind {
                ConnectionKind::Server { .. } => state.view.angles(
                    time,
                    state.intermission.as_ref(),
                    state.velocity,
                    self.idle_vars()?,
                    self.kick_vars()?,
                    self.roll_vars()?,
                ),

                ConnectionKind::Demo(_) => {
                    let v = state.entities[state.view_entity_id()].angles;
                    Angles {
                        pitch: -v.x,
                        yaw: v.y,
                        roll: -v.z,
                    }
                }
            },

            None => Err(ClientError::NotConnected)?,
        };

        Ok(angles)
    }

    pub fn view_ent(&self) -> Result<usize, ClientError> {
        match self.conn {
            Some(Connection { ref state, .. }) => Ok(state.view.entity_id()),
            None => Err(ClientError::NotConnected),
        }
    }

    pub fn time(&self) -> Result<Duration, ClientError> {
        match self.conn {
            Some(Connection { ref state, .. }) => Ok(state.time),
            None => Err(ClientError::NotConnected),
        }
    }

    pub fn get_lerp_factor(&mut self) -> Result<f32, ClientError> {
        match self.conn {
            Some(Connection { ref state, .. }) => Ok(state.lerp_factor),
            None => Err(ClientError::NotConnected),
        }
    }

    pub fn frame(&mut self, frame_time: Duration) -> Result<(), ClientError> {
        let cl_nolerp = self.cvar_value("cl_nolerp")?;
        let sv_gravity = self.cvar_value("sv_gravity")?;
        let kick_vars = self.kick_vars()?;
        if let Some(ref mut conn) = self.conn {
            conn.frame(
                frame_time,
                &self.vfs,
                &mut self.cmds.borrow_mut(),
                &mut self.console.borrow_mut(),
                &self.audio_device,
                kick_vars,
                cl_nolerp,
                sv_gravity,
            )?;
        }

        Ok(())
    }

    pub fn iter_visible_entities(&self) -> Option<impl Iterator<Item = &ClientEntity> + Clone> {
        self.conn
            .as_ref()
            .map(|conn| conn.state.iter_visible_entities())
    }

    pub fn iter_lights(&self) -> Result<impl Iterator<Item = &Light>, ClientError> {
        match self.conn {
            Some(Connection { ref state, .. }) => Ok(state.lights.iter()),
            None => Err(ClientError::NotConnected),
        }
    }

    pub fn iter_particles(&self) -> Result<impl Iterator<Item = &Particle>, ClientError> {
        match self.conn {
            Some(Connection { ref state, .. }) => Ok(state.particles.iter()),
            None => Err(ClientError::NotConnected),
        }
    }

    pub fn intermission(&self) -> Result<Option<&IntermissionKind>, ClientError> {
        match self.conn {
            Some(Connection { ref state, .. }) => Ok(state.intermission.as_ref()),
            None => Err(ClientError::NotConnected),
        }
    }

    pub fn start_time(&self) -> Result<Duration, ClientError> {
        match self.conn {
            Some(Connection { ref state, .. }) => Ok(state.start_time),
            None => Err(ClientError::NotConnected),
        }
    }

    pub fn completion_time(&self) -> Result<Option<Duration>, ClientError> {
        match self.conn {
            Some(Connection { ref state, .. }) => Ok(state.completion_time),
            None => Err(ClientError::NotConnected),
        }
    }

    pub fn items(&self) -> Result<ItemFlags, ClientError> {
        match self.conn {
            Some(Connection { ref state, .. }) => Ok(state.items),
            None => Err(ClientError::NotConnected),
        }
    }

    pub fn item_get_time(&self) -> Result<&[Duration; net::MAX_ITEMS], ClientError> {
        match self.conn {
            Some(Connection { ref state, .. }) => Ok(&state.item_get_time),
            None => Err(ClientError::NotConnected),
        }
    }

    pub fn weapon(&self) -> Result<i32, ClientError> {
        match self.conn {
            Some(Connection { ref state, .. }) => Ok(state.stats[ClientStat::Weapon as usize]),
            None => Err(ClientError::NotConnected),
        }
    }

    pub fn active_weapon(&self) -> Result<i32, ClientError> {
        match self.conn {
            Some(Connection { ref state, .. }) => {
                Ok(state.stats[ClientStat::ActiveWeapon as usize])
            }
            None => Err(ClientError::NotConnected),
        }
    }

    pub fn stats(&self) -> Result<&[i32; MAX_STATS], ClientError> {
        match self.conn {
            Some(Connection { ref state, .. }) => Ok(&state.stats),
            None => Err(ClientError::NotConnected),
        }
    }

    pub fn face_anim_time(&self) -> Result<Duration, ClientError> {
        match self.conn {
            Some(Connection { ref state, .. }) => Ok(state.face_anim_time),
            None => Err(ClientError::NotConnected),
        }
    }

    pub fn lightstyle_values(&self) -> Result<Vec<f32>, ClientError> {
        match self.conn {
            Some(Connection { ref state, .. }) => {
                let mut values = Vec::new();

                for lightstyle_id in 0..64 {
                    match state.light_styles.get(&lightstyle_id) {
                        Some(ls) => {
                            let float_time = engine::duration_to_f32(state.time);
                            let frame = if ls.len() == 0 {
                                None
                            } else {
                                Some((float_time * 10.0) as usize % ls.len())
                            };

                            values.push(match frame {
                                // 'z' - 'a' = 25, so divide by 12.5 to get range [0, 2]
                                Some(f) => (ls.as_bytes()[f] - 'a' as u8) as f32 / 12.5,
                                None => 1.0,
                            })
                        }

                        None => Err(ClientError::NoSuchLightmapAnimation(lightstyle_id as usize))?,
                    }
                }

                Ok(values)
            }

            None => Err(ClientError::NotConnected),
        }
    }

    pub fn color_shift(&self) -> [f32; 4] {
        match self.conn {
            Some(Connection { ref state, .. }) => {
                state.color_shifts.iter().fold([0.0; 4], |accum, elem| {
                    let elem_a = elem.borrow().percent as f32 / 255.0 / 2.0;
                    if elem_a == 0.0 {
                        return accum;
                    }
                    let in_a = accum[3];
                    let out_a = in_a + elem_a * (1.0 - in_a);
                    let color_factor = elem_a / out_a;

                    let mut out = [0.0; 4];
                    for i in 0..3 {
                        out[i] = accum[i] * (1.0 - color_factor)
                            + elem.borrow().dest_color[i] as f32 / 255.0 * color_factor;
                    }
                    out[3] = out_a.min(1.0).max(0.0);
                    out
                })
            }

            None => [0.0; 4],
        }
    }

    fn move_vars(&self) -> Result<MoveVars, ClientError> {
        Ok(MoveVars {
            cl_anglespeedkey: self.cvar_value("cl_anglespeedkey")?,
            cl_pitchspeed: self.cvar_value("cl_pitchspeed")?,
            cl_yawspeed: self.cvar_value("cl_yawspeed")?,
            cl_sidespeed: self.cvar_value("cl_sidespeed")?,
            cl_upspeed: self.cvar_value("cl_upspeed")?,
            cl_forwardspeed: self.cvar_value("cl_forwardspeed")?,
            cl_backspeed: self.cvar_value("cl_backspeed")?,
            cl_movespeedkey: self.cvar_value("cl_movespeedkey")?,
        })
    }

    fn idle_vars(&self) -> Result<IdleVars, ClientError> {
        Ok(IdleVars {
            v_idlescale: self.cvar_value("v_idlescale")?,
            v_ipitch_cycle: self.cvar_value("v_ipitch_cycle")?,
            v_ipitch_level: self.cvar_value("v_ipitch_level")?,
            v_iroll_cycle: self.cvar_value("v_iroll_cycle")?,
            v_iroll_level: self.cvar_value("v_iroll_level")?,
            v_iyaw_cycle: self.cvar_value("v_iyaw_cycle")?,
            v_iyaw_level: self.cvar_value("v_iyaw_level")?,
        })
    }

    fn kick_vars(&self) -> Result<KickVars, ClientError> {
        Ok(KickVars {
            v_kickpitch: self.cvar_value("v_kickpitch")?,
            v_kickroll: self.cvar_value("v_kickroll")?,
            v_kicktime: self.cvar_value("v_kicktime")?,
        })
    }

    fn mouse_vars(&self) -> Result<MouseVars, ClientError> {
        Ok(MouseVars {
            m_pitch: self.cvar_value("m_pitch")?,
            m_yaw: self.cvar_value("m_yaw")?,
            sensitivity: self.cvar_value("sensitivity")?,
        })
    }

    fn roll_vars(&self) -> Result<RollVars, ClientError> {
        Ok(RollVars {
            cl_rollangle: self.cvar_value("cl_rollangle")?,
            cl_rollspeed: self.cvar_value("cl_rollspeed")?,
        })
    }

    pub fn trace<'a, I>(&self, entity_ids: I) -> Result<TraceFrame, ClientError>
    where
        I: IntoIterator<Item = &'a usize>,
    {
        match self.conn {
            Some(Connection { ref state, .. }) => {
                let mut trace = TraceFrame {
                    msg_times_ms: [
                        state.msg_times[0].num_milliseconds(),
                        state.msg_times[1].num_milliseconds(),
                    ],
                    time_ms: state.time.num_milliseconds(),
                    lerp_factor: state.lerp_factor,
                    entities: HashMap::new(),
                };

                for id in entity_ids.into_iter() {
                    let ent = &state.entities[*id];

                    let msg_origins = [ent.msg_origins[0].into(), ent.msg_origins[1].into()];
                    let msg_angles_deg = [
                        [
                            ent.msg_angles[0][0].0,
                            ent.msg_angles[0][1].0,
                            ent.msg_angles[0][2].0,
                        ],
                        [
                            ent.msg_angles[1][0].0,
                            ent.msg_angles[1][1].0,
                            ent.msg_angles[1][2].0,
                        ],
                    ];

                    trace.entities.insert(
                        *id as u32,
                        TraceEntity {
                            msg_origins,
                            msg_angles_deg,
                            origin: ent.origin.into(),
                        },
                    );
                }

                Ok(trace)
            }

            None => Err(ClientError::NotConnected),
        }
    }
}

impl std::ops::Drop for Client {
    fn drop(&mut self) {
        // if this errors, it was already removed so we don't care
        let _ = self.cmds.borrow_mut().remove("reconnect");
    }
}
