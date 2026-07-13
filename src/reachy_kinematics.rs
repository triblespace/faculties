//! Compile-time quarantine seam for the native Reachy experiment.
//!
//! The original draft copied Pollen Robotics' standalone kinematics source,
//! whose repository does not declare a license. That source is deliberately
//! not redistributed here. The exact draft remains in the local preservation
//! stash; a real implementation must be independently written or come from a
//! clearly licensed upstream crate before native serial access is enabled.

use nalgebra::{Matrix4, Vector3};

pub struct Kinematics;

impl Kinematics {
    pub fn new(_motor_arm_length: f64, _rod_length: f64) -> Self {
        Self
    }

    pub fn add_branch(
        &mut self,
        _branch_platform: Vector3<f64>,
        _t_world_motor: Matrix4<f64>,
        _solution: f64,
    ) {
    }

    pub fn reset_forward_kinematics(&mut self, _t_world_platform: Matrix4<f64>) {}

    pub fn forward_kinematics(
        &mut self,
        _joint_angles: Vec<f64>,
        _body_yaw: Option<f64>,
    ) -> Matrix4<f64> {
        panic!("quarantined Reachy kinematics has no implementation")
    }

    pub fn inverse_kinematics_safe(
        &mut self,
        _t_world_platform: Matrix4<f64>,
        _body_yaw: Option<f64>,
        _max_relative_yaw: Option<f64>,
        _max_body_yaw: Option<f64>,
    ) -> Vec<f64> {
        panic!("quarantined Reachy kinematics has no implementation")
    }
}
