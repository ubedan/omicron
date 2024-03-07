// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Disk related types

use anyhow::bail;
use camino::{Utf8Path, Utf8PathBuf};
use derive_more::From;
use illumos_utils::zpool::{ZpoolKind, ZpoolName};
use key_manager::StorageKeyRequester;
use omicron_common::api::external::Generation;
use omicron_common::disk::DiskIdentity;
use omicron_common::ledger::Ledgerable;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sled_hardware::{
    DiskVariant, Partition, PooledDisk, PooledDiskError, UnparsedDisk,
};
use slog::Logger;
use uuid::Uuid;

use crate::config::MountConfig;
use crate::dataset;

#[derive(
    Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq, Hash,
)]
pub struct OmicronPhysicalDiskConfig {
    pub identity: DiskIdentity,
    pub id: Uuid,
    pub pool_id: Uuid,
}

#[derive(
    Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq, Hash,
)]
pub struct OmicronPhysicalDisksConfig {
    /// generation number of this configuration
    ///
    /// This generation number is owned by the control plane (i.e., RSS or
    /// Nexus, depending on whether RSS-to-Nexus handoff has happened).  It
    /// should not be bumped within Sled Agent.
    ///
    /// Sled Agent rejects attempts to set the configuration to a generation
    /// older than the one it's currently running.
    pub generation: Generation,

    pub disks: Vec<OmicronPhysicalDiskConfig>,
}

impl Ledgerable for OmicronPhysicalDisksConfig {
    fn is_newer_than(&self, other: &OmicronPhysicalDisksConfig) -> bool {
        self.generation > other.generation
    }

    // No need to do this, the generation number is provided externally.
    fn generation_bump(&mut self) {}
}

impl OmicronPhysicalDisksConfig {
    pub fn new() -> Self {
        Self { generation: Generation::new(), disks: vec![] }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DiskError {
    #[error(transparent)]
    Dataset(#[from] crate::dataset::DatasetError),
    #[error(transparent)]
    PooledDisk(#[from] sled_hardware::PooledDiskError),
}

/// A synthetic disk which has been formatted with a zpool.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SyntheticDisk {
    raw: RawSyntheticDisk,
    zpool_name: ZpoolName,
}

impl SyntheticDisk {
    // "Manages" a SyntheticDisk by ensuring that it has a Zpool and importing
    // it. If the zpool already exists, it is imported, but not re-created.
    pub fn new(
        log: &Logger,
        raw: RawSyntheticDisk,
        zpool_id: Option<Uuid>,
    ) -> Self {
        let zpool_name = sled_hardware::disk::ensure_zpool_exists(
            log,
            raw.variant,
            &raw.path,
            zpool_id,
        )
        .unwrap();
        sled_hardware::disk::ensure_zpool_imported(log, &zpool_name).unwrap();
        sled_hardware::disk::ensure_zpool_failmode_is_continue(
            log,
            &zpool_name,
        )
        .unwrap();

        Self { raw, zpool_name }
    }
}

// A synthetic disk that acts as one "found" by the hardware and that is backed
// by a zpool
#[derive(Debug, Clone, PartialEq, Eq, Hash, Ord, PartialOrd)]
pub struct RawSyntheticDisk {
    pub path: Utf8PathBuf,
    pub identity: DiskIdentity,
    pub variant: DiskVariant,
}

impl RawSyntheticDisk {
   /// Creates the file with a specified length, and also parses it as
    /// a [RawSyntheticDisk].
    pub fn new_with_length<P: AsRef<Utf8Path>>(
        vdev: P,
        length: u64,
    ) -> Result<Self, anyhow::Error> {
        let file = std::fs::File::create(vdev.as_ref())?;
        file.set_len(length)?;
        Self::new(vdev)
    }

    pub fn new<P: AsRef<Utf8Path>>(vdev: P) -> Result<Self, anyhow::Error> {
        let path = vdev.as_ref();
        let Some(file) = path.file_name() else {
            bail!("Missing file name for synthetic disk");
        };

        let Some(file) = file.strip_suffix(".vdev") else {
            bail!("Missing '.vdev' suffix for synthetic disk");
        };

        let (serial, variant) = if let Some(serial) = file.strip_prefix("m2_") {
            (serial, DiskVariant::M2)
        } else if let Some(serial) = file.strip_prefix("u2_") {
            (serial, DiskVariant::U2)
        } else {
            bail!("Unknown file prefix: {file}. Try one of {{m2_,u2_}}");
        };

        let identity = DiskIdentity {
            vendor: "synthetic-vendor".to_string(),
            serial: format!("synthetic-serial-{serial}"),
            model: format!("synthetic-model-{variant:?}"),
        };

        Ok(Self { path: path.into(), identity, variant })
    }
}

// An [`UnparsedDisk`] disk learned about from the hardware or a wrapped zpool
#[derive(Debug, Clone, PartialEq, Eq, Hash, Ord, PartialOrd, From)]
pub enum RawDisk {
    Real(UnparsedDisk),
    Synthetic(RawSyntheticDisk),
}

impl RawDisk {
    pub fn is_boot_disk(&self) -> bool {
        match self {
            Self::Real(disk) => disk.is_boot_disk(),
            Self::Synthetic(disk) => {
                // Just label any M.2 the boot disk.
                disk.variant == DiskVariant::M2
            }
        }
    }

    pub fn identity(&self) -> &DiskIdentity {
        match self {
            Self::Real(disk) => &disk.identity(),
            Self::Synthetic(disk) => &disk.identity,
        }
    }

    pub fn variant(&self) -> DiskVariant {
        match self {
            Self::Real(disk) => disk.variant(),
            Self::Synthetic(disk) => disk.variant,
        }
    }

    pub fn is_synthetic(&self) -> bool {
        match self {
            Self::Real(_) => false,
            Self::Synthetic(_) => true,
        }
    }

    pub fn is_real(&self) -> bool {
        !self.is_synthetic()
    }

    pub fn devfs_path(&self) -> &Utf8PathBuf {
        match self {
            Self::Real(disk) => disk.devfs_path(),
            Self::Synthetic(_) => unreachable!(),
        }
    }
}

/// A physical [`PooledDisk`] or a [`SyntheticDisk`] that contains or is backed
/// by a single zpool and that has provisioned datasets. This disk is ready for
/// usage by higher level software.
#[derive(Debug, Clone, PartialEq, Eq, Hash, From)]
pub enum Disk {
    Real(PooledDisk),
    Synthetic(SyntheticDisk),
}

impl Disk {
    pub async fn new(
        log: &Logger,
        mount_config: &MountConfig,
        raw_disk: RawDisk,
        pool_id: Option<Uuid>,
        key_requester: Option<&StorageKeyRequester>,
    ) -> Result<Self, DiskError> {
        let disk: Disk = match raw_disk {
            RawDisk::Real(disk) => PooledDisk::new(log, disk, pool_id)?.into(),
            RawDisk::Synthetic(disk) => {
                Disk::Synthetic(SyntheticDisk::new(log, disk, pool_id))
            }
        };
        dataset::ensure_zpool_has_datasets(
            log,
            mount_config,
            disk.zpool_name(),
            disk.identity(),
            key_requester,
        )
        .await?;

        if matches!(disk.variant(), DiskVariant::U2) {
            dataset::ensure_zpool_datasets_are_encrypted(
                log,
                disk.zpool_name(),
            )
            .await
            .map_err(|err| crate::dataset::DatasetError::from(err))?;
        }

        Ok(disk)
    }

    pub fn is_synthetic(&self) -> bool {
        match self {
            Self::Real(_) => false,
            Self::Synthetic(_) => true,
        }
    }

    pub fn is_real(&self) -> bool {
        !self.is_synthetic()
    }

    pub fn is_boot_disk(&self) -> bool {
        match self {
            Self::Real(disk) => disk.is_boot_disk,
            Self::Synthetic(disk) => {
                // Just label any M.2 the boot disk.
                disk.raw.variant == DiskVariant::M2
            }
        }
    }

    pub fn identity(&self) -> &DiskIdentity {
        match self {
            Self::Real(disk) => &disk.identity,
            Self::Synthetic(disk) => &disk.raw.identity,
        }
    }

    pub fn variant(&self) -> DiskVariant {
        match self {
            Self::Real(disk) => disk.variant,
            Self::Synthetic(disk) => match disk.zpool_name.kind() {
                ZpoolKind::External => DiskVariant::U2,
                ZpoolKind::Internal => DiskVariant::M2,
            },
        }
    }

    pub fn devfs_path(&self) -> &Utf8PathBuf {
        match self {
            Self::Real(disk) => &disk.paths.devfs_path,
            Self::Synthetic(_) => unreachable!(),
        }
    }

    pub fn zpool_name(&self) -> &ZpoolName {
        match self {
            Self::Real(disk) => &disk.zpool_name,
            Self::Synthetic(disk) => &disk.zpool_name,
        }
    }

    pub fn boot_image_devfs_path(
        &self,
        raw: bool,
    ) -> Result<Utf8PathBuf, PooledDiskError> {
        match self {
            Self::Real(disk) => disk.paths.partition_device_path(
                &disk.partitions,
                Partition::BootImage,
                raw,
            ),
            Self::Synthetic(_) => unreachable!(),
        }
    }

    pub fn dump_device_devfs_path(
        &self,
        raw: bool,
    ) -> Result<Utf8PathBuf, PooledDiskError> {
        match self {
            Self::Real(disk) => disk.paths.partition_device_path(
                &disk.partitions,
                Partition::DumpDevice,
                raw,
            ),
            Self::Synthetic(_) => unreachable!(),
        }
    }

    pub fn slot(&self) -> i64 {
        match self {
            Self::Real(disk) => disk.slot,
            Self::Synthetic(_) => unreachable!(),
        }
    }
}

impl From<Disk> for RawDisk {
    fn from(disk: Disk) -> RawDisk {
        match disk {
            Disk::Real(pooled_disk) => RawDisk::Real(UnparsedDisk::new(
                pooled_disk.paths.devfs_path,
                pooled_disk.paths.dev_path,
                pooled_disk.slot,
                pooled_disk.variant,
                pooled_disk.identity,
                pooled_disk.is_boot_disk,
            )),
            Disk::Synthetic(synthetic_disk) => {
                RawDisk::Synthetic(synthetic_disk.raw)
            }
        }
    }
}
