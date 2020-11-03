use crate::{
    comp::{HealthChange, HealthSource, Loadout},
    sync::Uid,
    util::Dir,
};
use serde::{Deserialize, Serialize};
use vek::*;

pub const BLOCK_EFFICIENCY: f32 = 0.9;

/// Each section of this struct determines what damage is applied to a
/// particular target, using some identifier
#[derive(Copy, Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Damages {
    /// Targets enemies, and all other creatures not in your group
    pub enemy: Option<Damage>,
    /// Targets people in the same group as you, and any pets you have
    pub group: Option<Damage>,
}

impl Damages {
    pub fn new(enemy: Option<Damage>, group: Option<Damage>) -> Self { Damages { enemy, group } }

    pub fn get_damage(self, same_group: bool) -> Option<Damage> {
        if same_group { self.group } else { self.enemy }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Damage {
    Melee(f32),
    Healing(f32),
    Projectile(f32),
    Explosion(f32),
    Falling(f32),
    Shockwave(f32),
    Energy(f32),
}

impl Damage {
    pub fn modify_damage(
        self,
        block: bool,
        loadout: Option<&Loadout>,
        uid: Option<Uid>,
    ) -> HealthChange {
        match self {
            Damage::Melee(damage) => {
                let mut damage = damage;
                // Critical hit
                let mut critdamage = 0.0;
                if rand::random() {
                    critdamage = damage * 0.3;
                }
                // Block
                if block {
                    damage *= 1.0 - BLOCK_EFFICIENCY
                }
                // Armor
                let damage_reduction = loadout.map_or(0.0, |l| l.get_damage_reduction());
                damage *= 1.0 - damage_reduction;

                // Critical damage applies after armor for melee
                if (damage_reduction - 1.0).abs() > f32::EPSILON {
                    damage += critdamage;
                }

                HealthChange {
                    amount: -damage as i32,
                    cause: HealthSource::Attack { by: uid.unwrap() },
                }
            },
            Damage::Projectile(damage) => {
                let mut damage = damage;
                // Critical hit
                if rand::random() {
                    damage *= 1.2;
                }
                // Block
                if block {
                    damage *= 1.0 - BLOCK_EFFICIENCY
                }
                // Armor
                let damage_reduction = loadout.map_or(0.0, |l| l.get_damage_reduction());
                damage *= 1.0 - damage_reduction;

                HealthChange {
                    amount: -damage as i32,
                    cause: HealthSource::Projectile { owner: uid },
                }
            },
            Damage::Explosion(damage) => {
                let mut damage = damage;
                // Block
                if block {
                    damage *= 1.0 - BLOCK_EFFICIENCY
                }
                // Armor
                let damage_reduction = loadout.map_or(0.0, |l| l.get_damage_reduction());
                damage *= 1.0 - damage_reduction;

                HealthChange {
                    amount: -damage as i32,
                    cause: HealthSource::Explosion { owner: uid },
                }
            },
            Damage::Shockwave(damage) => {
                let mut damage = damage;
                // Armor
                let damage_reduction = loadout.map_or(0.0, |l| l.get_damage_reduction());
                damage *= 1.0 - damage_reduction;

                HealthChange {
                    amount: -damage as i32,
                    cause: HealthSource::Attack { by: uid.unwrap() },
                }
            },
            Damage::Energy(damage) => {
                let mut damage = damage;
                // Armor
                let damage_reduction = loadout.map_or(0.0, |l| l.get_damage_reduction());
                damage *= 1.0 - damage_reduction;

                HealthChange {
                    amount: -damage as i32,
                    cause: HealthSource::Energy { owner: uid },
                }
            },
            Damage::Healing(heal) => HealthChange {
                amount: heal as i32,
                cause: HealthSource::Healing { by: uid },
            },
            Damage::Falling(damage) => {
                let mut damage = damage;
                // Armor
                let damage_reduction = loadout.map_or(0.0, |l| l.get_damage_reduction());
                if (damage_reduction - 1.0).abs() < f32::EPSILON {
                    damage = 0.0;
                }
                HealthChange {
                    amount: -damage as i32,
                    cause: HealthSource::World,
                }
            },
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Knockback {
    Away(f32),
    Towards(f32),
    Up(f32),
    TowardsUp(f32),
}

impl Knockback {
    pub fn calculate_impulse(self, dir: Dir) -> Vec3<f32> {
        match self {
            Knockback::Away(strength) => strength * *Dir::slerp(dir, Dir::new(Vec3::unit_z()), 0.5),
            Knockback::Towards(strength) => {
                strength * *Dir::slerp(-dir, Dir::new(Vec3::unit_z()), 0.5)
            },
            Knockback::Up(strength) => strength * Vec3::unit_z(),
            Knockback::TowardsUp(strength) => {
                strength * *Dir::slerp(-dir, Dir::new(Vec3::unit_z()), 0.85)
            },
        }
    }
}