// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Discovered and usable disks and zpools

use crate::dataset::M2_DEBUG_DATASET;
use crate::disk::{Disk, RawDisk};
use crate::error::Error;
use crate::pool::Pool;
use camino::Utf8PathBuf;
use cfg_if::cfg_if;
use illumos_utils::zpool::ZpoolName;
use omicron_common::disk::DiskIdentity;
use sled_hardware::DiskVariant;
use std::collections::BTreeMap;
use std::sync::Arc;

// The directory within the debug dataset in which bundles are created.
const BUNDLE_DIRECTORY: &str = "bundle";

// The directory for zone bundles.
const ZONE_BUNDLE_DIRECTORY: &str = "zone";

pub enum AddDiskResult {
    DiskInserted,
    DiskAlreadyInserted,
    DiskQueued,
}

impl AddDiskResult {
    pub fn disk_inserted(&self) -> bool {
        match self {
            AddDiskResult::DiskInserted => true,
            _ => false,
        }
    }
}

// The Sled Agent is responsible for both observing disks and managing them at
// the request of the broader control plane. This enum encompasses that duality,
// by representing all disks that can exist, managed or not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManagedDisk {
    // A disk managed by the control plane.
    Managed {
        disk: Disk,
        // TODO: I think you could maybe remove this??
        // TODO: Does it really make sense to co-locate the zpool here?
        pool: Pool,
    },
    // A disk which has been observed by the sled, but which is not yet being
    // managed by the control plane.
    //
    // This disk should be treated as "read-only" until we're explicitly told to
    // use it.
    Unmanaged(RawDisk),
}

/// Storage related resources: disks and zpools
///
/// This state is internal to the [`crate::manager::StorageManager`] task. Clones
/// of this state can be retrieved by requests to the `StorageManager` task
/// from the [`crate::manager::StorageHandle`]. This state is not `Sync`, and
/// as such does not require any mutexes. However, we do expect to share it
/// relatively frequently, and we want copies of it to be as cheaply made
/// as possible. So any large state is stored inside `Arc`s. On the other
/// hand, we expect infrequent updates to this state, and as such, we use
/// [`std::sync::Arc::make_mut`] to implement clone on write functionality
/// inside the `StorageManager` task if there are any outstanding copies.
/// Therefore, we only pay the cost to update infrequently, and no locks are
/// required by callers when operating on cloned data. The only contention here
/// is for the reference counters of the internal Arcs when `StorageResources`
/// gets cloned or dropped.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StorageResources {
    // All disks, real and synthetic, being managed by this sled
    disks: Arc<BTreeMap<DiskIdentity, ManagedDisk>>,
}

impl StorageResources {
    /// Returns an iterator over all disks, managed or not.
    pub fn all_disks(&self) -> impl Iterator<Item = (&DiskIdentity, DiskVariant)> {
        self.disks.iter().map(|(identity, disk)| {
            match disk {
                ManagedDisk::Managed { disk, ..  } => (identity, disk.variant()),
                ManagedDisk::Unmanaged(raw) => (identity, raw.variant()),
            }
        })
    }

    /// Returns an iterator over all managed disks.
    pub fn managed_disks(&self) -> impl Iterator<Item = (&DiskIdentity, &Disk)> {
        self.disks.iter().filter_map(|(identity, disk)| {
            match disk {
                ManagedDisk::Managed { disk, .. } => Some((identity, disk)),
                _ => None
            }
        })
    }

    pub(crate) fn get_disk(
        &self,
        disk_identity: &DiskIdentity,
    ) -> Result<&ManagedDisk, Error> {
        let Some(disk) = self.disks.get(disk_identity) else {
            return Err(Error::PhysicalDiskNotFound);
        };
        Ok(disk)
    }

    /// Updates the known set of resources to include a new "unmanaged" disk,
    /// unless that disk already exists.
    pub(crate) fn insert_raw_disk(
        &mut self,
        disk: RawDisk,
    ) -> Result<AddDiskResult, Error> {
        let disk_id = disk.identity().clone();
        if self.disks.contains_key(&disk_id) {
            return Ok(AddDiskResult::DiskAlreadyInserted);
        }
        Arc::make_mut(&mut self.disks).insert(disk_id, ManagedDisk::Unmanaged(disk));
        Ok(AddDiskResult::DiskInserted)
    }

    /// Insert a disk and its zpool
    ///
    /// If the disk passed in is new or modified, or its pool size or pool
    /// name changed, then insert the changed values and return `DiskInserted`.
    /// Otherwise, do not insert anything and return `DiskAlreadyInserted`.
    /// For instance, if only the pool health changes, because it is not one
    /// of the checked values, we will not insert the update and will return
    /// `DiskAlreadyInserted`.
    pub(crate) fn insert_managed_disk(
        &mut self,
        disk: Disk,
    ) -> Result<AddDiskResult, Error> {
        let disk_id = disk.identity().clone();
        let zpool_name = disk.zpool_name().clone();
        let zpool = Pool::new(zpool_name, disk_id.clone())?;
        if let Some(entry) = self.disks.get(&disk_id) {
            if let ManagedDisk::Managed { disk: stored_disk, pool: stored_pool } = entry {
                if stored_disk == &disk
                    && stored_pool.info.size() == zpool.info.size()
                    && stored_pool.name == zpool.name
                {
                    return Ok(AddDiskResult::DiskAlreadyInserted);
                }
            }
        }
        // Either the disk or zpool changed
        Arc::make_mut(&mut self.disks).insert(disk_id, ManagedDisk::Managed { disk, pool: zpool });
        Ok(AddDiskResult::DiskInserted)
    }

    pub(crate) fn unmanage_disk(
        &mut self,
        disk_identity: DiskIdentity,
    ) -> Result<(), Error> {
        let disks = Arc::make_mut(&mut self.disks);
        let Some(_entry) = disks.get_mut(&disk_identity) else {
            return Err(Error::PhysicalDiskNotFound);
        };

        todo!();
    }

    /// Insert a disk while creating a fake pool
    /// This is a workaround for current mock based testing strategies
    /// in the sled-agent.
    #[cfg(feature = "testing")]
    pub fn insert_fake_disk(&mut self, disk: Disk) -> AddDiskResult {
        let disk_id = disk.identity().clone();
        let zpool_name = disk.zpool_name().clone();
        let zpool = Pool::new_with_fake_info(zpool_name, disk_id.clone());
        if self.disks.contains_key(&disk_id) {
            return AddDiskResult::DiskAlreadyInserted;
        }
        // Either the disk or zpool changed
        Arc::make_mut(&mut self.disks).insert(disk_id, ManagedDisk::Managed { disk, pool: zpool });
        AddDiskResult::DiskInserted
    }

    /// Delete a disk and its zpool
    ///
    /// Return true, if data was changed, false otherwise
    ///
    /// Note: We never allow removal of synthetic disks in production as they
    /// are only added once.
    pub(crate) fn remove_disk(&mut self, id: &DiskIdentity) -> bool {
        let Some(entry) = self.disks.get(id) else {
            return false;
        };
        let synthetic = match entry {
            ManagedDisk::Managed { disk, .. } => {
                disk.is_synthetic()
            },
            ManagedDisk::Unmanaged(raw) => {
                raw.is_synthetic()
            }
        };

        cfg_if! {
            if #[cfg(test)] {
                // For testing purposes, we allow synthetic disks to be deleted.
                // Silence an unused variable warning.
                _ = synthetic;
            } else {
                // In production, we disallow removal of synthetic disks as they
                // are only added once.
                if synthetic {
                    return false;
                }
            }
        }

        // Safe to unwrap as we just checked the key existed above
        Arc::make_mut(&mut self.disks).remove(id).unwrap();
        true
    }

    /// Returns the identity of the boot disk.
    ///
    /// If this returns `None`, we have not processed the boot disk yet.
    pub fn boot_disk(&self) -> Option<(DiskIdentity, ZpoolName)> {
        for (id, disk) in self.disks.iter() {
            if let ManagedDisk::Managed { disk, .. } = disk {
                if disk.is_boot_disk() {
                    return Some((id.clone(), disk.zpool_name().clone()));
                }
            }
        }
        None
    }

    /// Returns all M.2 zpools
    pub fn all_m2_zpools(&self) -> Vec<ZpoolName> {
        self.all_zpools(DiskVariant::M2)
    }

    /// Returns all U.2 zpools
    pub fn all_u2_zpools(&self) -> Vec<ZpoolName> {
        self.all_zpools(DiskVariant::U2)
    }

    /// Returns all mountpoints within all M.2s for a particular dataset.
    pub fn all_m2_mountpoints(&self, dataset: &str) -> Vec<Utf8PathBuf> {
        self.all_m2_zpools()
            .iter()
            .map(|zpool| zpool.dataset_mountpoint(dataset))
            .collect()
    }

    /// Returns all mountpoints within all U.2s for a particular dataset.
    pub fn all_u2_mountpoints(&self, dataset: &str) -> Vec<Utf8PathBuf> {
        self.all_u2_zpools()
            .iter()
            .map(|zpool| zpool.dataset_mountpoint(dataset))
            .collect()
    }

    /// Returns all zpools managed by the control plane
    pub fn get_all_zpools(&self) -> Vec<(ZpoolName, DiskVariant)> {
        self.disks
            .values()
            .filter_map(|disk| {
                if let ManagedDisk::Managed { disk, .. } = disk {
                    Some((disk.zpool_name().clone(), disk.variant()))
                } else {
                    None
                }
            })
            .collect()
    }

    // Returns all zpools of a particular variant.
    //
    // Only returns zpools from disks actively being managed.
    fn all_zpools(&self, variant: DiskVariant) -> Vec<ZpoolName> {
        self.disks
            .values()
            .filter_map(|disk| {
                if let ManagedDisk::Managed { disk, .. } = disk {
                    if disk.variant() == variant {
                        return Some(disk.zpool_name().clone());
                    }
                }
                None
            })
            .collect()
    }

    /// Return the directories for storing zone service bundles.
    pub fn all_zone_bundle_directories(&self) -> Vec<Utf8PathBuf> {
        self.all_m2_mountpoints(M2_DEBUG_DATASET)
            .into_iter()
            .map(|p| p.join(BUNDLE_DIRECTORY).join(ZONE_BUNDLE_DIRECTORY))
            .collect()
    }
}
