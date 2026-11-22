//! Bodies as struct-of-arrays, gathered from rapier's RigidBodySet every tick and
//! written back after the solve.
//!
//! rapier's set stays the store of record (physics.rs, the Humanoid code and the
//! queries read poses from it); this is the solver's working copy. Slots are dense
//! and rebuilt by `gather` into buffers that keep their capacity (no allocation
//! per tick once warm). State that must survive between ticks (per-body level,
//! sleep counters, last solve's motion for the arbiter) lives in `persist`,
//! indexed by rapier's arena index and checked against the handle's generation.

use glam::{Mat3, Quat, Vec3};
use rapier3d::prelude as rp;

pub const NONE: u32 = u32::MAX;

/// Flags per slot.
pub mod flag {
    pub const DYNAMIC: u8 = 1;
    pub const KINEMATIC: u8 = 2;
    /// Taking part in this tick's solve (STEP decides; see `Persist::asleep`).
    pub const AWAKE: u8 = 4;
    /// Moved by this tick (pose / velocity must be written back).
    pub const DIRTY: u8 = 8;
    /// `mat` / `com` are Studio's own pose state (kept in `Persist::pose`), not a
    /// conversion of rapier's quaternion (STEP §4.2 [L2]).
    pub const STUDIO_POSE: u8 = 16;
}

/// Studio's f32 3×3 rotation, row-major as a CoordinateFrame keeps it (STEP §4.2 [L2]),
/// with the operations in the binary's order.
pub mod m3 {
    use glam::{Mat3, Vec3};
    pub type M3 = [f32; 9];
    #[inline]
    pub fn from_glam(m: &Mat3) -> M3 {
        [m.x_axis.x, m.y_axis.x, m.z_axis.x, m.x_axis.y, m.y_axis.y, m.z_axis.y, m.x_axis.z, m.y_axis.z, m.z_axis.z]
    }
    #[inline]
    pub fn to_glam(a: &M3) -> Mat3 {
        Mat3::from_cols(Vec3::new(a[0], a[3], a[6]), Vec3::new(a[1], a[4], a[7]), Vec3::new(a[2], a[5], a[8]))
    }
    /// Studio (A·B): each row of the result = (a_i1·B_1 + a_i0·B_0) + a_i2·B_2 (packed).
    #[inline]
    pub fn mul(a: &M3, b: &M3) -> M3 {
        let mut o = [0.0f32; 9];
        for i in 0..3 {
            for j in 0..3 {
                o[3 * i + j] = (a[3 * i + 1] * b[3 + j] + a[3 * i] * b[j]) + a[3 * i + 2] * b[6 + j];
            }
        }
        o
    }
    /// A·v as Studio do it: per row the packed product summed
    /// pairwise, = (a_i0·v0 + a_i1·v1) + a_i2·v2 (the fourth lane is 0).
    #[inline]
    pub fn mul_vec(a: &M3, v: Vec3) -> Vec3 {
        let r = |i: usize| (a[3 * i] * v.x + a[3 * i + 1] * v.y) + a[3 * i + 2] * v.z;
        Vec3::new(r(0), r(1), r(2))
    }
    #[inline]
    fn norm(v: Vec3) -> f32 {
        ((v.x * v.x + v.y * v.y) + v.z * v.z).sqrt()
    }
    /// rot(θ) Studio: n = |θ|; n ≤ 0 → I; else axis = θ·(1/n), renormalised by
    /// Studio (·(1/|axis|)), then Studio(axis, n): c = cosf n, s = sinf n,
    /// t = 1 − c; diagonal (u_k·u_k)·t + c, off-diagonal (u_i·u_j)·t ∓ u_k·s.
    pub fn rot(t: Vec3) -> M3 {
        let n = norm(t);
        if !(0.0 < n) {
            return [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0];
        }
        let k = 1.0 / n;
        let a = Vec3::new(t.x * k, t.y * k, t.z * k);
        let k2 = 1.0 / norm(a);
        let (x, y, z) = (a.x * k2, a.y * k2, a.z * k2);
        let (c, s) = (n.cos(), n.sin());
        let t = 1.0 - c;
        let (xy, xz, yz) = ((y * x) * t, (z * x) * t, (z * y) * t);
        [
            (x * x) * t + c,
            xy - z * s,
            y * s + xz,
            z * s + xy,
            (y * y) * t + c,
            yz - x * s,
            xz - y * s,
            x * s + yz,
            (z * z) * t + c,
        ]
    }
}

/// Per-body state kept between ticks (indexed by rapier arena index): the
/// SolverRigidBody fields the legacy step reads (STEP §0.1).
#[derive(Debug, Clone, Copy)]
pub struct Persist {
    pub generation: u32,
    pub live: bool,
    /// Current level L, body (ctor 1; set by Studio when the body's
    /// level list is due, STEP §1.2 step 6 / §3.5).
    pub level: u8,
    /// The level list the body is in (STEP §0.3): moved by a type-1 change
    /// and, after a non-empty queue, to its island's level by the island rebuild
    /// Studio → lambda Studio (STEP §3.6 [C4]).
    pub list_level: u8,
    /// The body's node level in the island graph, graph[node] (Studio reads
    /// it as uint; written by Studio for type-1 and type-3 changes, STEP §3.6
    /// [C4]): islands take the minimum of these, not of the lists.
    pub node_level: u8,
    /// Level change pending, body (STEP §3.2 / §3.6).
    pub pending: bool,
    /// Level requests by slot, body.., and the previous ones,
    /// body.. (STEP §3.3).
    pub requests: [u8; 7],
    pub prev_requests: [u8; 7],
    /// Debounce counter in ticks, body (STEP §3.5 / §3.6).
    pub debounce: i32,
    /// Condition-number estimate, body (ctor 1.0; STEP §3.3a).
    pub cond: f32,
    /// CFL number body and position-correction factor body
    /// (Studio, STEP §4.2b).
    pub cfl: f32,
    pub pcf: f32,
    pub asleep: bool,
    /// Sleep bookkeeping (STEP §sleep).
    pub quiet_ticks: u32,
    /// Assembly sleep state, history and activity (sleep.rs, SLEEP.md).
    pub sleep: super::sleep::Sleep,
    /// Geometric-stiffness column norms, body.. (JOINT D1.4 [J2]): written by
    /// the row collection of a solve, read and cleared by
    /// the next solve's free velocity (Studio via Studio).
    pub gs: Vec3,
    /// The Solver::Force accumulators body (F) / (τ) (JOINT I.14, STEP §4.0):
    /// "Apply forces" adds to them every tick a Force item fires (A on its own ticks, B's
    /// reaction on A's), the body's free velocity reads them and its write-back clears them
    /// — so they hold until the body's own solve.
    pub f_solver: Vec3,
    pub t_solver: Vec3,
    /// Studio's pose state (body: R row-major, CoM x; STEP §4.2 [L2]) and the
    /// rapier pose it was written as; used while rapier still holds that pose.
    pub pose: Option<StudioPose>,
}

#[derive(Debug, Clone, Copy)]
pub struct StudioPose {
    pub r: m3::M3,
    pub x: Vec3,
    pub written_pos: Vec3,
    pub written_rot: Quat,
}

impl Default for Persist {
    fn default() -> Self {
        Persist {
            generation: 0,
            live: false,
            level: 1,
            list_level: 1,
            node_level: 1,
            pending: false,
            requests: [0; 7],
            prev_requests: [0; 7],
            debounce: 0,
            cond: 1.0,
            cfl: 0.0,
            pcf: 0.0,
            asleep: false,
            quiet_ticks: 0,
            sleep: Default::default(),
            gs: Vec3::ZERO,
            f_solver: Vec3::ZERO,
            t_solver: Vec3::ZERO,
            pose: None,
        }
    }
}

#[derive(Default)]
pub struct BodySet {
    // -- per slot (dense, this tick) --
    pub handle: Vec<rp::RigidBodyHandle>,
    pub flags: Vec<u8>,
    /// Body frame (rapier's body pose).
    pub pos: Vec<Vec3>,
    pub rot: Vec<Quat>,
    /// The rotation as the solver integrates it: Studio's f32 matrix when the body
    /// has one (flag::STUDIO_POSE), else rapier's quaternion as a matrix.
    pub mat: Vec<Mat3>,
    /// Centre of mass in the body frame and in the world.
    pub local_com: Vec<Vec3>,
    pub com: Vec<Vec3>,
    /// Velocity of the centre of mass and angular velocity (world).
    pub linvel: Vec<Vec3>,
    pub angvel: Vec<Vec3>,
    pub inv_mass: Vec<f32>,
    /// Inverse principal inertia and the principal frame (body frame).
    pub inv_inertia: Vec<Vec3>,
    pub inertia_frame: Vec<Quat>,
    /// Studio's body-frame inertia I (body) when the body has a Studio mass record.
    pub inertia_body: Vec<Option<Mat3>>,
    /// World inverse inertia tensor (from rot · inertia_frame), filled by `gather`.
    pub inv_inertia_world: Vec<Mat3>,
    /// External force / torque for this tick (user forces from the movers,
    /// physics.rs `apply_movers`, the Humanoid's `humanoid_forces`).
    pub force: Vec<Vec3>,
    pub torque: Vec<Vec3>,
    pub gravity_scale: Vec<f32>,
    /// Kinematic target pose (rapier's next_position).
    pub next_pos: Vec<Vec3>,
    pub next_rot: Vec<Quat>,
    /// Island of the slot this tick (island.rs), NONE if not solved.
    pub island: Vec<u32>,
    /// The slot-0 box (STEP §3.4): centre relative to the centre of mass and half
    /// extents, body frame; filled for solved bodies by step.rs `boxes`.
    pub box_c: Vec<Vec3>,
    pub box_e: Vec<Vec3>,
    // -- maps --
    /// rapier arena index -> slot (NONE when not gathered this tick).
    pub slot_of: Vec<u32>,
    pub persist: Vec<Persist>,
    /// Studio mass records by body (physics.rs → StudioSolver::note_mass; STEP §5 [L2]).
    pub studio_mass: std::collections::HashMap<rp::RigidBodyHandle, super::mass::PartMass>,
    /// The assembly's part Size boxes' union, body frame (physics.rs → note_box; STEP §3.4 [L3]).
    pub size_box: std::collections::HashMap<rp::RigidBodyHandle, (Vec3, Vec3)>,
    /// body, the length r of Studio (physics.rs → note_char_len; SLEEP §2.1 [S2]).
    pub char_len: std::collections::HashMap<rp::RigidBodyHandle, f32>,
}

impl BodySet {
    pub fn len(&self) -> usize {
        self.handle.len()
    }

    fn clear(&mut self) {
        for h in &self.handle {
            let i = h.into_raw_parts().0 as usize;
            if let Some(s) = self.slot_of.get_mut(i) {
                *s = NONE;
            }
        }
        self.handle.clear();
        self.flags.clear();
        self.pos.clear();
        self.rot.clear();
        self.mat.clear();
        self.local_com.clear();
        self.com.clear();
        self.linvel.clear();
        self.angvel.clear();
        self.inv_mass.clear();
        self.inv_inertia.clear();
        self.inertia_frame.clear();
        self.inv_inertia_world.clear();
        self.inertia_body.clear();
        self.force.clear();
        self.torque.clear();
        self.gravity_scale.clear();
        self.next_pos.clear();
        self.next_rot.clear();
        self.island.clear();
        self.box_c.clear();
        self.box_e.clear();
    }

    /// Slot of a rapier body this tick.
    #[inline]
    pub fn slot(&self, h: rp::RigidBodyHandle) -> u32 {
        self.slot_of.get(h.into_raw_parts().0 as usize).copied().unwrap_or(NONE)
    }

    /// Copies every enabled dynamic and kinematic body out of rapier.
    pub fn gather(&mut self, bodies: &rp::RigidBodySet) {
        // STEP §5 / §5a: mass, centre and inertia are the assembly's;
        // rustudio's come from part_mass per part (physics.rs), summed by rapier.
        // The composition and per-shape inertia are STEP Open 4.
        self.clear();
        for (h, b) in bodies.iter() {
            if !b.is_enabled() || !b.is_dynamic_or_kinematic() {
                continue;
            }
            let (idx, gen) = h.into_raw_parts();
            let idx = idx as usize;
            if self.slot_of.len() <= idx {
                self.slot_of.resize(idx + 1, NONE);
                self.persist.resize(idx + 1, Persist::default());
            }
            let p = &mut self.persist[idx];
            if !p.live || p.generation != gen {
                *p = Persist { generation: gen, live: true, ..Default::default() };
            }
            // Sleep is sleep.rs's (state 5 = out of the solve); rapier's sleeping flag
            // mirrors it, and a property write that wakes the body in rapier
            // (physics.rs wake_part / set_velocity) is SLEEP §7.2's property wake.
            let asleep = b.is_dynamic() && p.sleep.gather(b.is_sleeping(), v(b.linvel()), v(b.angvel()), v(b.position().translation), q(b.position().rotation));
            p.asleep = asleep;
            let slot = self.handle.len() as u32;
            self.slot_of[idx] = slot;
            let mp = b.mass_properties().local_mprops;
            let pose = b.position();
            let rot = q(pose.rotation);
            let frame = q(mp.principal_inertia_local_frame);
            // Locked rotations (rustudio's upright characters): no angular response.
            let inv_i = if b.locked_axes().contains(rp::LockedAxes::ROTATION_LOCKED) { Vec3::ZERO } else { v(mp.inv_principal_inertia) };
            let mut f = if b.is_dynamic() { flag::DYNAMIC } else { flag::KINEMATIC };
            // STEP §4.2 [L2]: Studio's body keeps its own R / x between solves; rapier's
            // pose is only a copy. Kept while rapier still holds what was written.
            let tr = v(pose.translation);
            let studio = match p.pose {
                Some(sp) if sp.written_pos == tr && sp.written_rot == rot => Some(sp),
                _ => {
                    p.pose = None;
                    None
                }
            };
            if studio.is_some() {
                f |= flag::STUDIO_POSE;
            }
            if !asleep {
                f |= flag::AWAKE;
            }
            self.handle.push(h);
            self.flags.push(f);
            self.pos.push(v(pose.translation));
            self.rot.push(rot);
            self.local_com.push(v(mp.local_com));
            match studio {
                Some(sp) => {
                    self.mat.push(m3::to_glam(&sp.r));
                    self.com.push(sp.x);
                }
                None => {
                    self.mat.push(Mat3::from_quat(rot));
                    self.com.push(v(pose.translation) + rot * v(mp.local_com));
                }
            }
            self.linvel.push(v(b.linvel()));
            self.angvel.push(v(b.angvel()));
            self.inv_mass.push(if b.is_dynamic() { mp.inv_mass } else { 0.0 });
            self.inv_inertia.push(if b.is_dynamic() { inv_i } else { Vec3::ZERO });
            self.inertia_frame.push(frame);
            let rm = *self.mat.last().unwrap();
            // STEP §5 [L2]: a body with Studio's mass record takes its 1/m,
            // I (body), I⁻¹ = Studio(I) (; glam's cofactor inverse, the
            // f32 order of Studio is not transcribed) and Iw⁻¹ = R·I⁻¹·Rᵀ.
            let sm = if b.is_dynamic() && !b.locked_axes().contains(rp::LockedAxes::ROTATION_LOCKED) { self.studio_mass.get(&h) } else { None };
            match sm {
                Some(sm) => {
                    let i = sm.inertia();
                    let inv = if i.determinant().is_finite() && i.determinant() != 0.0 { i.inverse() } else { Mat3::ZERO };
                    *self.inv_mass.last_mut().unwrap() = 1.0 / sm.mass;
                    self.inertia_body.push(Some(i));
                    self.inv_inertia_world.push(rm * inv * rm.transpose());
                }
                None => {
                    self.inertia_body.push(None);
                    self.inv_inertia_world.push(world_inv_inertia_m(rm * Mat3::from_quat(frame), if b.is_dynamic() { inv_i } else { Vec3::ZERO }));
                }
            }
            self.force.push(v(b.user_force()));
            self.torque.push(v(b.user_torque()));
            self.gravity_scale.push(b.gravity_scale());
            let np = b.next_position();
            self.next_pos.push(v(np.translation));
            self.next_rot.push(q(np.rotation));
            self.island.push(NONE);
            self.box_c.push(Vec3::ZERO);
            self.box_e.push(Vec3::ZERO);
        }
    }

    /// Writes the moved slots back: velocities, body pose, collider poses.
    pub fn scatter(&mut self, bodies: &mut rp::RigidBodySet, colliders: &mut rp::ColliderSet) {
        for s in 0..self.len() {
            if self.flags[s] & flag::DIRTY == 0 {
                continue;
            }
            let Some(b) = bodies.get_mut(self.handle[s]) else { continue };
            if b.is_dynamic() {
                b.set_linvel(rv(self.linvel[s]), false);
                b.set_angvel(rv(self.angvel[s]), false);
            }
            let (mut pp, mut r) = (self.pos[s], self.rot[s]);
            // The part pose through Studio's codec (STEP §4.2 [L3], Studio with
            // DFFlagSAuthLocallyCompressPVsSolver and cfg): opt-in, the gate is off in
            // every recording (`codec_on`). The body keeps its exact state (GEOM2 §B).
            if codec_on() {
                let m = m3::from_glam(&self.mat[s]);
                let d = super::codec::roundtrip(&super::codec::Pv { r: m, p: [pp.x, pp.y, pp.z], v: [0.0; 3], w: [0.0; 3] });
                pp = Vec3::new(d.p[0], d.p[1], d.p[2]);
                r = Quat::from_mat3(&m3::to_glam(&d.r)).normalize();
            }
            b.set_position(rp::Pose::from_parts(rv(pp), rp::Rotation::from_xyzw(r.x, r.y, r.z, r.w)), false);
            // Studio's pose stays the solver's (STEP §4.2 [L2]); rapier keeps its copy.
            let idx = self.handle[s].into_raw_parts().0 as usize;
            let pose = b.position();
            self.persist[idx].pose = if self.flags[s] & flag::STUDIO_POSE != 0 {
                Some(StudioPose { r: m3::from_glam(&self.mat[s]), x: self.com[s], written_pos: v(pose.translation), written_rot: q(pose.rotation) })
            } else {
                None
            };
        }
        // Collider poses now, not at the next tick's collision step: the Humanoid's
        // floor rays and the scripts' queries run between ticks.
        bodies.propagate_modified_body_positions_to_colliders(colliders);
    }
}

/// `RUSTUDIO_SOLVER_CODEC=1`: part poses written through Studio's pose codec (codec.rs).
/// Off by default: write-back Studio applies it only when cfg is set (Studio:
/// solver, written by the solver vtbl[31] Studio, a server-authority switch),
/// and 8 of 2285 rows of wedge_drop / hinge_free / stack3 are codec fixed points (tick 0).
pub fn codec_on() -> bool {
    static K: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *K.get_or_init(|| std::env::var("RUSTUDIO_SOLVER_CODEC").is_ok_and(|v| v == "1"))
}

/// F · diag(inv_i) · Fᵀ for a frame matrix F.
#[inline]
pub fn world_inv_inertia_m(r: Mat3, inv_i: Vec3) -> Mat3 {
    r * Mat3::from_diagonal(inv_i) * r.transpose()
}

/// R · diag(inv_i) · Rᵀ.
#[inline]
pub fn world_inv_inertia(frame: Quat, inv_i: Vec3) -> Mat3 {
    let r = Mat3::from_quat(frame);
    r * Mat3::from_diagonal(inv_i) * r.transpose()
}

#[inline]
pub fn v(x: rp::Vector) -> Vec3 {
    Vec3::new(x.x, x.y, x.z)
}

#[inline]
pub fn rv(x: Vec3) -> rp::Vector {
    rp::Vector::new(x.x, x.y, x.z)
}

#[inline]
pub fn q(r: rp::Rotation) -> Quat {
    Quat::from_xyzw(r.x, r.y, r.z, r.w)
}
