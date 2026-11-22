//! The part of rustudio every other crate sits on: the document model, math
//! and mechanics, the shared data types, the asset cache, terrain data,
//! logging and benchmarking. The core tests build with
//! `cargo test -p rustudio-core`.
//! It does depend on `rustudio-account` for the calls that need the session
//! (asset and place downloads, the avatar's description).
//!
//! What is not added here: anything that draws a frame, runs Luau or steps
//! physics, anything that only the editor or the player needs, and any
//! reference to the crates above this one — they do not compile.

pub mod grass_stream;
pub mod sim_clock;
pub mod heap;
pub mod load_trace;
pub mod log;
pub mod studio_content;
pub mod paths;
pub mod platform;
pub mod assets;
pub mod bench;
pub mod collision_groups;
pub mod crash;
pub mod default_scripts;
pub mod doc;
pub mod editable_source;
pub mod attachment;
pub mod game_settings;
pub mod rustudio_settings;
pub mod fbx;
pub mod fbx_anim;
pub mod api;
pub mod api_stubs;
pub mod telemetry;
pub mod math;
pub mod mech;
pub mod mesh;
pub mod body;
pub mod truss;
pub mod mesh_bvh;
pub mod project_values;
pub mod cage_rbf;
pub mod terrain;
pub mod terrain_region;
pub mod terrain_mat;
pub mod texture;
pub mod texture_pack;
pub mod vertex;
pub mod rig_type;
