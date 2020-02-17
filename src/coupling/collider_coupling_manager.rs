use crate::coupling::CouplingManager;
use crate::geometry::{HGrid, HGridEntry};
use crate::math::{Point, Vector};
use crate::object::Fluid;
use crate::object::{Boundary, BoundaryHandle};
use na::{RealField, Unit};
use ncollide::bounding_volume::BoundingVolume;
use ncollide::query::PointQuery;
use ncollide::shape::FeatureId;
use nphysics::math::ForceType;
use nphysics::object::{BodySet, ColliderAnchor, ColliderHandle, ColliderSet};
use std::collections::HashMap;

/// The way a collider is coupled to a boundary object.
pub enum CouplingMethod<N: RealField> {
    /// The collider shape is approximated with the given sample points in local-space.
    ///
    /// It is recommanded that those points are separated by a distance smaller or equal to twice
    /// the particle radius used to initialize the LiquidWorld.
    StaticSampling(Vec<Point<N>>),
    /// The colliser shape is approximated by a dynamic set of points automatically computed based on contacts with fluid particles.
    DynamicContactSampling,
}

struct ColliderCouplingEntry<N: RealField> {
    coupling_method: CouplingMethod<N>,
    boundary: BoundaryHandle,
    features: Vec<FeatureId>,
}

/// Structure managing all the coupling between colliders from nphysics with boundaries and fluids from salva.
pub struct ColliderCouplingSet<N: RealField, CollHandle: ColliderHandle> {
    entries: HashMap<CollHandle, ColliderCouplingEntry<N>>,
}

impl<N: RealField, CollHandle: ColliderHandle> ColliderCouplingSet<N, CollHandle> {
    /// Create a new collider coupling manager.
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    /// Register a coupling between a boundary and a collider.
    pub fn register_coupling(
        &mut self,
        boundary: BoundaryHandle,
        collider: CollHandle,
        coupling_method: CouplingMethod<N>,
    ) {
        let _ = self.entries.insert(
            collider,
            ColliderCouplingEntry {
                coupling_method,
                boundary,
                features: Vec::new(),
            },
        );
    }

    pub fn as_manager_mut<'a, Colliders, Bodies>(
        &'a mut self,
        colliders: &'a Colliders,
        bodies: &'a mut Bodies,
    ) -> ColliderCouplingManager<N, Colliders, Bodies>
    where
        Colliders: ColliderSet<N, Bodies::Handle, Handle = CollHandle>,
        Bodies: BodySet<N>,
    {
        ColliderCouplingManager {
            coupling: self,
            colliders,
            bodies,
        }
    }
}

pub struct ColliderCouplingManager<'a, N: RealField, Colliders, Bodies>
where
    N: RealField,
    Colliders: ColliderSet<N, Bodies::Handle>,
    Bodies: BodySet<N>,
{
    coupling: &'a mut ColliderCouplingSet<N, Colliders::Handle>,
    colliders: &'a Colliders,
    bodies: &'a mut Bodies,
}

impl<'a, N, Colliders, Bodies> CouplingManager<N>
    for ColliderCouplingManager<'a, N, Colliders, Bodies>
where
    N: RealField,
    Colliders: ColliderSet<N, Bodies::Handle>,
    Bodies: BodySet<N>,
{
    fn update_boundaries(
        &mut self,
        dt: N,
        h: N,
        hgrid: &HGrid<N, HGridEntry>,
        fluids: &mut [Fluid<N>],
        fluids_delta_vels: &mut [Vec<Vector<N>>],
        boundaries: &mut [Boundary<N>],
    ) {
        for (collider, coupling) in &mut self.coupling.entries {
            if let (Some(collider), Some(boundary)) = (
                self.colliders.get(*collider),
                boundaries.get_mut(coupling.boundary),
            ) {
                boundary.positions.clear();
                boundary.velocities.clear();
                coupling.features.clear();

                match &coupling.coupling_method {
                    CouplingMethod::StaticSampling(points) => {
                        for pt in points {
                            boundary.positions.push(collider.position() * pt);
                            // XXX: actually set the velocity of this point.
                            boundary.velocities.push(Vector::zeros());
                        }
                    }
                    CouplingMethod::DynamicContactSampling => {
                        let prediction = h; // * na::convert(0.5);
                        let collider_pos = collider.position();
                        let aabb = collider
                            .shape()
                            .aabb(&collider_pos)
                            .loosened(h + prediction);

                        for particle in hgrid
                            .cells_intersecting_aabb(aabb.mins(), aabb.maxs())
                            .flat_map(|e| e.1)
                        {
                            match particle {
                                HGridEntry::FluidParticle(fluid_id, particle_id) => {
                                    let fluid = &mut fluids[*fluid_id];
                                    let particle_delta =
                                        &mut fluids_delta_vels[*fluid_id][*particle_id];
                                    let particle_pos = fluid.positions[*particle_id]
                                        + (fluid.velocities[*particle_id] + *particle_delta) * dt;

                                    if aabb.contains_local_point(&particle_pos) {
                                        let (proj, feature) =
                                            collider.shape().project_point_with_feature(
                                                &collider_pos,
                                                &particle_pos,
                                            );

                                        let dpt = particle_pos - proj.point;

                                        if let Some((normal, depth)) =
                                            Unit::try_new_and_get(dpt, N::default_epsilon())
                                        {
                                            if proj.is_inside {
                                                fluid.positions[*particle_id] -=
                                                    *normal * (depth + na::convert(0.0001));
                                            } else if depth > h + prediction {
                                                continue;
                                            }
                                        }

                                        boundary.positions.push(proj.point);
                                        coupling.features.push(feature);
                                    }
                                }
                                HGridEntry::BoundaryParticle(..) => {
                                    // Not yet implemented.
                                }
                            }
                        }
                    }
                }

                boundary.clear_forces(true);
            }
        }
    }

    fn transmit_forces(&mut self, boundaries: &[Boundary<N>]) {
        for (collider, coupling) in &self.coupling.entries {
            if let (Some(collider), Some(boundary)) = (
                self.colliders.get(*collider),
                boundaries.get(coupling.boundary),
            ) {
                if boundary.positions.is_empty() {
                    continue;
                }

                let forces = boundary.forces.read().unwrap();

                match collider.anchor() {
                    ColliderAnchor::OnBodyPart { body_part, .. } => {
                        if let Some(body) = self.bodies.get_mut(body_part.0) {
                            for (pos, force) in
                                boundary.positions.iter().zip(forces.iter().cloned())
                            {
                                // FIXME: how do we deal with large density ratio?
                                // Is it only an issue with PBF?
                                // The following commented code was an attempt to limit the force applied
                                // to the bodies in order to avoid large forces.
                                //
                                //                                let ratio = na::convert::<_, N>(3.0)
                                //                                    * body.part(body_part.1).unwrap().inertia().mass();
                                //
                                //                                if ratio < na::convert(1.0) {
                                //                                    force *= ratio;
                                //                                }

                                body.apply_force_at_point(
                                    body_part.1,
                                    &force,
                                    pos,
                                    ForceType::Force,
                                    true,
                                )
                            }
                        }
                    }
                    ColliderAnchor::OnDeformableBody { body, body_parts } => {
                        if let Some(body) = self.bodies.get_mut(*body) {
                            for (feature, pos, force) in itertools::multizip((
                                coupling.features.iter(),
                                boundary.positions.iter(),
                                forces.iter(),
                            )) {
                                let subshape_id =
                                    collider.shape().subshape_containing_feature(*feature);
                                let part_id = if let Some(body_parts) = body_parts {
                                    body_parts[subshape_id]
                                } else {
                                    subshape_id
                                };

                                body.apply_force_at_point(
                                    part_id,
                                    &force,
                                    pos,
                                    ForceType::Force,
                                    true,
                                )
                            }
                        }
                    }
                }
            }
        }
    }
}
