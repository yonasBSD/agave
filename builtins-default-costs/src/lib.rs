#![cfg_attr(feature = "frozen-abi", feature(min_specialization))]
#![allow(clippy::arithmetic_side_effects)]
use {
    agave_feature_set::{self as feature_set},
    ahash::AHashMap,
    solana_pubkey::Pubkey,
    solana_sdk_ids::{
        bpf_loader, bpf_loader_deprecated, bpf_loader_upgradeable, compute_budget, ed25519_program,
        loader_v4, secp256k1_program, stake, system_program, vote,
    },
};

#[derive(Clone)]
pub struct MigratingBuiltinCost {
    core_bpf_migration_feature: Pubkey,
    // encoding positional information explicitly for migration feature item,
    // its value must be correctly corresponding to this object's position
    // in MIGRATING_BUILTINS_COSTS, otherwise a const validation
    // `validate_position(MIGRATING_BUILTINS_COSTS)` will fail at compile time.
    position: usize,
}

/// DEVELOPER: when a builtin is migrated to sbpf, please add its corresponding
/// migration feature ID to BUILTIN_INSTRUCTION_COSTS, and move it from
/// NON_MIGRATING_BUILTINS_COSTS to MIGRATING_BUILTINS_COSTS, so the builtin's
/// default cost can be determined properly based on feature status.
/// When migration completed, eg the feature gate is enabled everywhere, please
/// remove that builtin entry from MIGRATING_BUILTINS_COSTS.
#[derive(Clone)]
pub enum BuiltinCost {
    Migrating(MigratingBuiltinCost),
    NotMigrating,
}

impl BuiltinCost {
    fn core_bpf_migration_feature(&self) -> Option<&Pubkey> {
        match self {
            BuiltinCost::Migrating(MigratingBuiltinCost {
                core_bpf_migration_feature,
                ..
            }) => Some(core_bpf_migration_feature),
            BuiltinCost::NotMigrating => None,
        }
    }

    fn position(&self) -> Option<usize> {
        match self {
            BuiltinCost::Migrating(MigratingBuiltinCost { position, .. }) => Some(*position),
            BuiltinCost::NotMigrating => None,
        }
    }
}

/// Number of compute units for each built-in programs
///
/// DEVELOPER WARNING: This map CANNOT be modified without causing a
/// consensus failure because this map is used to calculate the compute
/// limit for transactions that don't specify a compute limit themselves as
/// of https://github.com/anza-xyz/agave/issues/2212.  It's also used to
/// calculate the cost of a transaction which is used in replay to enforce
/// block cost limits as of
/// https://github.com/solana-labs/solana/issues/29595.
static BUILTIN_INSTRUCTION_COSTS: std::sync::LazyLock<AHashMap<Pubkey, BuiltinCost>> =
    std::sync::LazyLock::new(|| {
        MIGRATING_BUILTINS_COSTS
            .iter()
            .chain(NON_MIGRATING_BUILTINS_COSTS.iter())
            .cloned()
            .collect()
    });
// DO NOT ADD MORE ENTRIES TO THIS MAP

/// DEVELOPER WARNING: please do not add new entry into MIGRATING_BUILTINS_COSTS or
/// NON_MIGRATING_BUILTINS_COSTS, do so will modify BUILTIN_INSTRUCTION_COSTS therefore
/// cause consensus failure. However, when a builtin started being migrated to core bpf,
/// it MUST be moved from NON_MIGRATING_BUILTINS_COSTS to MIGRATING_BUILTINS_COSTS, then
/// correctly furnishing `core_bpf_migration_feature`.
///
#[allow(dead_code)]
const TOTAL_COUNT_BUILTINS: usize = 10;
#[cfg(test)]
static_assertions::const_assert_eq!(
    MIGRATING_BUILTINS_COSTS.len() + NON_MIGRATING_BUILTINS_COSTS.len(),
    TOTAL_COUNT_BUILTINS
);

pub const MIGRATING_BUILTINS_COSTS: &[(Pubkey, BuiltinCost)] = &[(
    stake::id(),
    BuiltinCost::Migrating(MigratingBuiltinCost {
        core_bpf_migration_feature: feature_set::migrate_stake_program_to_core_bpf::id(),
        position: 0,
    }),
)];

const NON_MIGRATING_BUILTINS_COSTS: &[(Pubkey, BuiltinCost)] = &[
    (vote::id(), BuiltinCost::NotMigrating),
    (system_program::id(), BuiltinCost::NotMigrating),
    (compute_budget::id(), BuiltinCost::NotMigrating),
    (bpf_loader_upgradeable::id(), BuiltinCost::NotMigrating),
    (bpf_loader_deprecated::id(), BuiltinCost::NotMigrating),
    (bpf_loader::id(), BuiltinCost::NotMigrating),
    (loader_v4::id(), BuiltinCost::NotMigrating),
    (secp256k1_program::id(), BuiltinCost::NotMigrating),
    (ed25519_program::id(), BuiltinCost::NotMigrating),
];

/// A table of 256 booleans indicates whether the first `u8` of a Pubkey exists in
/// BUILTIN_INSTRUCTION_COSTS. If the value is true, the Pubkey might be a builtin key;
/// if false, it cannot be a builtin key. This table allows for quick filtering of
/// builtin program IDs without the need for hashing.
pub static MAYBE_BUILTIN_KEY: std::sync::LazyLock<[bool; 256]> = std::sync::LazyLock::new(|| {
    let mut temp_table: [bool; 256] = [false; 256];
    BUILTIN_INSTRUCTION_COSTS
        .keys()
        .for_each(|key| temp_table[key.as_ref()[0] as usize] = true);
    temp_table
});

pub enum BuiltinMigrationFeatureIndex {
    NotBuiltin,
    BuiltinNoMigrationFeature,
    BuiltinWithMigrationFeature(usize),
}

pub fn get_builtin_migration_feature_index(program_id: &Pubkey) -> BuiltinMigrationFeatureIndex {
    BUILTIN_INSTRUCTION_COSTS.get(program_id).map_or(
        BuiltinMigrationFeatureIndex::NotBuiltin,
        |builtin_cost| {
            builtin_cost.position().map_or(
                BuiltinMigrationFeatureIndex::BuiltinNoMigrationFeature,
                BuiltinMigrationFeatureIndex::BuiltinWithMigrationFeature,
            )
        },
    )
}

/// const function validates `position` correctness at compile time.
#[allow(dead_code)]
const fn validate_position(migrating_builtins: &[(Pubkey, BuiltinCost)]) {
    let mut index = 0;
    while index < migrating_builtins.len() {
        match migrating_builtins[index].1 {
            BuiltinCost::Migrating(MigratingBuiltinCost { position, .. }) => assert!(
                position == index,
                "migration feture must exist and at correct position"
            ),
            BuiltinCost::NotMigrating => {
                panic!("migration feture must exist and at correct position")
            }
        }
        index += 1;
    }
}
const _: () = validate_position(MIGRATING_BUILTINS_COSTS);

/// Helper function to return ref of migration feature Pubkey at position `index`
/// from MIGRATING_BUILTINS_COSTS
pub fn get_migration_feature_id(index: usize) -> &'static Pubkey {
    MIGRATING_BUILTINS_COSTS
        .get(index)
        .expect("valid index of MIGRATING_BUILTINS_COSTS")
        .1
        .core_bpf_migration_feature()
        .expect("migrating builtin")
}

#[cfg(feature = "dev-context-only-utils")]
pub fn get_migration_feature_position(feature_id: &Pubkey) -> usize {
    MIGRATING_BUILTINS_COSTS
        .iter()
        .position(|(_, c)| c.core_bpf_migration_feature().expect("migrating builtin") == feature_id)
        .unwrap()
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_const_builtin_cost_arrays() {
        // sanity check to make sure built-ins are declared in the correct array
        assert!(MIGRATING_BUILTINS_COSTS
            .iter()
            .enumerate()
            .all(|(index, (_, c))| {
                c.core_bpf_migration_feature().is_some() && c.position() == Some(index)
            }));
        assert!(NON_MIGRATING_BUILTINS_COSTS
            .iter()
            .all(|(_, c)| c.core_bpf_migration_feature().is_none()));
    }

    #[test]
    fn test_get_builtin_migration_feature_index() {
        assert!(matches!(
            get_builtin_migration_feature_index(&Pubkey::new_unique()),
            BuiltinMigrationFeatureIndex::NotBuiltin
        ));
        assert!(matches!(
            get_builtin_migration_feature_index(&compute_budget::id()),
            BuiltinMigrationFeatureIndex::BuiltinNoMigrationFeature,
        ));
        let feature_index = get_builtin_migration_feature_index(&stake::id());
        assert!(matches!(
            feature_index,
            BuiltinMigrationFeatureIndex::BuiltinWithMigrationFeature(_)
        ));
        let BuiltinMigrationFeatureIndex::BuiltinWithMigrationFeature(feature_index) =
            feature_index
        else {
            panic!("expect migrating builtin")
        };
        assert_eq!(
            get_migration_feature_id(feature_index),
            &feature_set::migrate_stake_program_to_core_bpf::id()
        );
    }

    #[test]
    #[should_panic(expected = "valid index of MIGRATING_BUILTINS_COSTS")]
    fn test_get_migration_feature_id_invalid_index() {
        let _ = get_migration_feature_id(MIGRATING_BUILTINS_COSTS.len() + 1);
    }
}
