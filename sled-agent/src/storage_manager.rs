// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Management of sled-local storage.

use crate::illumos::{
    zfs::Mountpoint,
    zone::ZONE_PREFIX,
    zpool::ZpoolInfo,
};
use crate::illumos::running_zone::RunningZone;
use crate::vnic::{IdAllocator, Vnic};
use futures::stream::FuturesOrdered;
use futures::StreamExt;
use lazy_static::lazy_static;
use nexus_client::types::{DatasetKind, DatasetPutRequest, ZpoolPutRequest};
use omicron_common::api::external::{ByteCount, ByteCountRangeError};
use omicron_common::backoff;
use slog::Logger;
use std::collections::HashMap;
use std::convert::TryFrom;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinHandle;
use uuid::Uuid;

#[cfg(not(test))]
use crate::illumos::{dladm::Dladm, zfs::Zfs, zone::Zones, zpool::Zpool};
#[cfg(test)]
use crate::illumos::{
    dladm::MockDladm as Dladm, zfs::MockZfs as Zfs, zone::MockZones as Zones,
    zpool::MockZpool as Zpool,
};

#[cfg(test)]
use crate::mocks::MockNexusClient as NexusClient;
#[cfg(not(test))]
use nexus_client::Client as NexusClient;

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error(transparent)]
    Datalink(#[from] crate::illumos::dladm::Error),

    #[error(transparent)]
    Zfs(#[from] crate::illumos::zfs::Error),

    #[error(transparent)]
    Zpool(#[from] crate::illumos::zpool::Error),

    #[error("Failed to create base zone: {0}")]
    BaseZoneCreation(crate::illumos::zone::Error),

    #[error("Failed to configure a zone: {0}")]
    ZoneConfiguration(crate::illumos::zone::Error),

    #[error("Failed to manage a running zone: {0}")]
    ZoneManagement(#[from] crate::illumos::running_zone::Error),

    #[error("Error parsing pool size: {0}")]
    BadPoolSize(#[from] ByteCountRangeError),

    #[error("Failed to parse as UUID: {0}")]
    Parse(uuid::Error),

    #[error("Timed out waiting for service: {0}")]
    Timeout(String),
}

/// A ZFS storage pool.
struct Pool {
    id: Uuid,
    info: ZpoolInfo,
    // ZFS filesytem UUID -> Zone.
    zones: HashMap<Uuid, RunningZone>,
}

impl Pool {
    /// Queries for an existing Zpool by name.
    ///
    /// Returns Ok if the pool exists.
    fn new(name: &str) -> Result<Pool, Error> {
        let info = Zpool::get_info(name)?;

        // NOTE: This relies on the name being a UUID exactly.
        // We could be more flexible...
        let id: Uuid = info.name().parse().map_err(|e| Error::Parse(e))?;
        Ok(Pool { id, info, zones: HashMap::new() })
    }

    /// Associate an already running zone with this pool object.
    ///
    /// Typically this is used when a dataset within the zone (identified
    /// by ID) has a running zone (e.g. Crucible, Cockroach) operating on
    /// behalf of that data.
    fn add_zone(&mut self, id: Uuid, zone: RunningZone) {
        self.zones.insert(id, zone);
    }

    /// Access a zone managing data within this pool.
    fn get_zone(&self, id: Uuid) -> Option<&RunningZone> {
        self.zones.get(&id)
    }

    /// Returns the ID of the pool itself.
    fn id(&self) -> Uuid {
        self.id
    }
}

#[derive(Debug)]
struct DatasetName {
    // A unique identifier for the Zpool on which the dataset is stored.
    pool_name: String,
    // A name for the dataset within the Zpool.
    dataset_name: String,
}

impl DatasetName {
    fn new(pool_name: &str, dataset_name: &str) -> Self {
        Self {
            pool_name: pool_name.to_string(),
            dataset_name: dataset_name.to_string(),
        }
    }

    fn full(&self) -> String {
        format!("{}/{}", self.pool_name, self.dataset_name)
    }
}

// Description of a dataset within a ZFS pool, which should be created
// by the Sled Agent.
struct PartitionInfo {
    name: String,
    data_directory: String,
    port: u16,
    kind: DatasetKind,
}

impl PartitionInfo {
    fn zone_prefix(&self) -> String {
        format!("{}{}_", ZONE_PREFIX, self.name)
    }
}

async fn ensure_running_zone(
    log: &Logger,
    vnic_id_allocator: &IdAllocator,
    partition_info: &PartitionInfo,
    dataset_name: &DatasetName,
) -> Result<RunningZone, Error> {
    match RunningZone::get(log, &partition_info.zone_prefix(), partition_info.port)
        .await
    {
        Ok(zone) => {
            info!(log, "Zone for {} is already running", dataset_name.full());
            Ok(zone)
        }
        Err(_) => {
            info!(log, "Zone for {} is not running (it may exist, but it's not running). Booting", dataset_name.full());
            let (nic, zname) = configure_zone(
                log,
                vnic_id_allocator,
                partition_info,
                dataset_name,
            )?;
            RunningZone::boot(log, zname, nic, partition_info.port)
                .await
                .map_err(|e| e.into())
        }
    }
}

// Creates a VNIC and configures a zone.
fn configure_zone(
    log: &Logger,
    vnic_id_allocator: &IdAllocator,
    partition_info: &PartitionInfo,
    dataset_name: &DatasetName,
) -> Result<(Vnic, String), Error> {
    let physical_dl = Dladm::find_physical()?;
    let nic = Vnic::new_control(vnic_id_allocator, &physical_dl, None)?;

    // The zone name is based on:
    // - A unique Oxide prefix ("oxz_")
    // - The name of the partition being hosted (e.g., "cockroachdb")
    // - Unique Zpool identifier (typically a UUID).
    //
    // This results in a zone name which is distinct across different zpools,
    // but stable and predictable across reboots.
    let zname = format!("{}{}", partition_info.zone_prefix(), dataset_name.pool_name);

    let zone_image = PathBuf::from(&format!("/opt/oxide/{}.tar.gz", partition_info.name));

    // Configure the new zone - this should be identical to the base zone,
    // but with a specified VNIC and pool.
    Zones::install_omicron_zone(
        log,
        &zname,
        &zone_image,
        &[
            zone::Dataset {
                name: dataset_name.full(),
            },
        ],
        &[],
        vec![nic.name().to_string()],
    )
    .map_err(|e| Error::ZoneConfiguration(e))?;
    Ok((nic, zname))
}

lazy_static! {
    static ref PARTITIONS: Vec<PartitionInfo> = vec![
        /*
        PartitionInfo {
            name: "crucible",
            data_directory: "/data",
            // TODO: Ensure crucible agent uses this port.
            // Currently, nothing is running in the zone, so it's made up.
            port: 8080,
            kind: DatasetKind::Crucible,
        },
        */
        PartitionInfo {
            name: "cockroachdb".to_string(),
            data_directory: "/data".to_string(),
            // TODO: Ensure cockroach uses this port.
            // Currently, nothing is running in the zone, so it's made up.
            port: 32221,
            kind: DatasetKind::Cockroach,
        },
    ];
}

// A worker that starts zones for pools as they are received.
struct StorageWorker {
    log: Logger,
    sled_id: Uuid,
    nexus_client: Arc<NexusClient>,
    pools: Arc<Mutex<HashMap<String, Pool>>>,
    new_pools_rx: mpsc::Receiver<String>,
    vnic_id_allocator: IdAllocator,
}

impl StorageWorker {
    // Idempotently ensure the named dataset exists as a filesystem with a UUID.
    //
    // Returns the UUID attached to the ZFS filesystem.
    fn ensure_dataset_with_id(dataset_name: &DatasetName) -> Result<Uuid, Error> {
        let fs_name = &dataset_name.full();
        Zfs::ensure_filesystem(&fs_name, Mountpoint::Path(PathBuf::from("/data")))?;
        // Ensure the dataset has a usable UUID.
        if let Ok(id_str) = Zfs::get_oxide_value(&fs_name, "uuid") {
            if let Ok(id) = id_str.parse::<Uuid>() {
                return Ok(id);
            }
        }
        let id = Uuid::new_v4();
        Zfs::set_oxide_value(&fs_name, "uuid", &id.to_string())?;
        Ok(id)
    }

    // Formats a partition within a zpool, starting a zone for it.
    // Returns the UUID attached to the underlying ZFS partition.
    //
    // For now, we place all "expected" datasets on each new zpool
    // we see. The decision of "whether or not to actually use the
    // dataset" is a decision left to both the bootstrapping protocol
    // and Nexus.
    //
    // If we had a better signal - from the bootstrapping system - about
    // where Cockroach nodes should exist, we could be more selective
    // about this placement.
    async fn initialize_partition(
        &self,
        pool: &mut Pool,
        partition_info: &PartitionInfo,
    ) -> Result<Uuid, Error> {
        let dataset_name = DatasetName::new(pool.info.name(), &partition_info.name);

        info!(&self.log, "[InitializePartition] Ensuring dataset {} exists", dataset_name.full());
        let id = StorageWorker::ensure_dataset_with_id(&dataset_name)?;

        info!(&self.log, "[InitializePartition] Creating zone for {}", dataset_name.full());
        let zone = ensure_running_zone(
            &self.log,
            &self.vnic_id_allocator,
            partition_info,
            &dataset_name,
        )
        .await?;
        info!(&self.log, "[InitializePartition] Zone {} with address {} is running", zone.name(), zone.address());

        // TODO: This function isn't named as something "CRDB specific".
        // How do we distinguish this from the "crucible setup"?

        info!(&self.log, "[InitializePartition] Importing CRDB Manifest");
        zone.run_cmd(
            &[
                crate::illumos::zone::SVCCFG,
                "import",
                "/var/svc/manifest/site/cockroachdb/manifest.xml"
            ]
        )?;

        zone.run_cmd(
            &[
                crate::illumos::zone::SVCCFG,
                "-s",
                "svc:system/illumos/cockroachdb",
                "setprop",
                &format!("config/listen_addr={}", zone.address().to_string()),
            ]
        )?;

        zone.run_cmd(
            &[
                crate::illumos::zone::SVCCFG,
                "-s",
                "svc:system/illumos/cockroachdb",
                "setprop",
                &format!("config/store={}", partition_info.data_directory),
            ]
        )?;

        // TODO: Set these addresses, use "start" instead of
        // "start-single-node".
        zone.run_cmd(
            &[
                crate::illumos::zone::SVCCFG,
                "-s",
                "svc:system/illumos/cockroachdb",
                "setprop",
                &format!("config/join_addrs={}", "unknown"),
            ]
        )?;

        // Refresh the manifest with the new properties we set,
        // so they become "effective" properties when the service is enabled.
        zone.run_cmd(
            &[
                crate::illumos::zone::SVCCFG,
                "-s",
                "svc:system/illumos/cockroachdb:default",
                "refresh",
            ]
        )?;

        zone.run_cmd(
            &[
                crate::illumos::zone::SVCADM,
                "enable",
                "-t",
                &format!("svc:/system/illumos/cockroachdb:default"),
            ]
        )?;

        // "What does omicron-dev do":
        // "start-single-node" vs "start"
        //
        // DB Console HTTP Requests:
        //   --http-addr=:0
        //
        // DB Node/Client Request Port:
        //   --listen-addr=127.0.0.1:32221
        //
        // Path to the postgresql URL:
        //   --listening-url-file=<file>
        //
        //   Can be a tempfile, seems optional? Happens to be co-located
        //   w/"store".
        //
        // Store:
        //   --store=PATH
        //
        //
        // TODO: New ones:
        // Use to ensure you're joining the right cluster
        //   --cluster-name=...

        // TODO: Populate DB
        // TODO: Decide whether or not to populate DB
        //
        // TODO: Maybe move the "initialize / populate" decision somewhere else?

//      tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;

//        info!(&self.log, "[InitializePartition] Populating CRDB");
//
//        zone.run_cmd(
//            &[
//                "/opt/oxide/cockroachdb/bin/cockroach",
//                "sql",
//                "--insecure",
//                "--host",
//                zone.address().to_string(),
//                "--file",
//                "/opt/oxide/cockroachdb/sql/dbwipe.sql",
//            ]
//        )?;
//
//        zone.run_cmd(
//            &[
//                "/opt/oxide/cockroachdb/bin/cockroach",
//                "sql",
//                "--insecure",
//                "--host",
//                zone.address().to_string(),
//                "--file",
//                "/opt/oxide/cockroachdb/sql/dbinit.sql",
//            ]
//        )?;

        info!(
            &self.log,
            "[InitializePartition] Set up zone {} for partition {} successfully",
            zone.name(),
            partition_info.name
        );
        pool.add_zone(id, zone);
        Ok(id)
    }

    // Small wrapper around `Self::do_work_internal` that ensures we always
    // emit info to the log when we exit.
    async fn do_work(&mut self) -> Result<(), Error> {
        self.do_work_internal()
            .await
            .map(|()| {
                info!(self.log, "StorageWorker exited successfully");
            })
            .map_err(|e| {
                warn!(self.log, "StorageWorker exited unexpectedly: {}", e);
                e
            })
    }

    async fn do_work_internal(&mut self) -> Result<(), Error> {
        info!(self.log, "StorageWorker creating storage base zone");
        // Create a base zone, from which all running storage zones are cloned.
        Zones::create_storage_base(&self.log)
            .map_err(|e| Error::BaseZoneCreation(e))?;
        info!(self.log, "StorageWorker creating storage base zone - DONE");

        let mut nexus_notifications = FuturesOrdered::new();

        loop {
            tokio::select! {
                _ = nexus_notifications.next(), if !nexus_notifications.is_empty() => {},
                Some(pool_name) = self.new_pools_rx.recv() => {
                    let mut pools = self.pools.lock().await;
                    let pool = pools.get_mut(&pool_name).unwrap();

                    info!(
                        &self.log,
                        "Storage manager processing zpool: {:#?}", pool.info
                    );

                    let size = ByteCount::try_from(pool.info.size())?;

                    // Initialize all sled-local state.
                    let mut partitions = vec![];
                    for partition in PARTITIONS.iter() {
                        // NOTE: I've noticed that a failure to set up *any*
                        // partitions takes the whole storage worker down here.
                        //
                        // TODO: Can we tolerate *some* faults? Maybe have a
                        // managed set of "known bad" partitions, and keep
                        // going?
                        let id = self.initialize_partition(pool, partition).await?;
                        // Unwrap safety: We just put this zone in the pool.
                        let zone = pool.get_zone(id).unwrap();
                        partitions.push((id, zone.address(), partition.kind.clone()));
                    }

                    // Notify Nexus of the zpool and all datasets within.
                    let pool_id = pool.id();
                    let sled_id = self.sled_id;
                    let nexus = self.nexus_client.clone();
                    let notify_nexus = move || {
                        let zpool_request = ZpoolPutRequest { size: size.into() };
                        let nexus = nexus.clone();
                        let partitions = partitions.clone();
                        async move {
                            nexus
                                .zpool_put(&sled_id, &pool_id, &zpool_request)
                                .await
                                .map_err(backoff::BackoffError::Transient)?;

                            for (id, address, kind) in partitions {
                                let request = DatasetPutRequest {
                                    address: address.to_string(),
                                    kind,
                                };
                                nexus
                                    .dataset_put(&pool_id, &id, &request)
                                    .await
                                    .map_err(backoff::BackoffError::Transient)?;
                            }

                            Ok::<(), backoff::BackoffError<anyhow::Error>>(())
                        }
                    };
                    let log = self.log.clone();
                    let log_post_failure = move |error, delay| {
                        warn!(
                            log,
                            "failed to notify nexus, will retry in {:?}", delay;
                            "error" => ?error,
                        );
                    };
                    nexus_notifications.push(
                        backoff::retry_notify(
                            backoff::internal_service_policy(),
                            notify_nexus,
                            log_post_failure,
                        )
                    );
                },
            }
        }
    }
}

/// A sled-local view of all attached storage.
pub struct StorageManager {
    // A map of "zpool name" to "pool".
    pools: Arc<Mutex<HashMap<String, Pool>>>,
    new_pools_tx: mpsc::Sender<String>,

    // A handle to a worker which updates "pools".
    task: JoinHandle<Result<(), Error>>,
}

impl StorageManager {
    /// Creates a new [`StorageManager`] which should manage local storage.
    pub async fn new(
        log: &Logger,
        sled_id: Uuid,
        nexus_client: Arc<NexusClient>,
    ) -> Result<Self, Error> {
        let log = log.new(o!("component" => "sled agent storage manager"));
        let pools = Arc::new(Mutex::new(HashMap::new()));
        let (new_pools_tx, new_pools_rx) = mpsc::channel(10);
        let mut worker = StorageWorker {
            log,
            sled_id,
            nexus_client,
            pools: pools.clone(),
            new_pools_rx,
            vnic_id_allocator: IdAllocator::new(),
        };
        Ok(StorageManager {
            pools,
            new_pools_tx,
            task: tokio::task::spawn(async move { worker.do_work().await }),
        })
    }

    /// Adds a zpool to the storage manager.
    pub async fn upsert_zpool(&self, name: &str) -> Result<(), Error> {
        let zpool = Pool::new(name)?;

        let is_new = {
            let mut pools = self.pools.lock().await;
            let entry = pools.entry(name.to_string());
            let is_new =
                matches!(entry, std::collections::hash_map::Entry::Vacant(_));

            // Ensure that the pool info is up-to-date.
            entry
                .and_modify(|e| {
                    e.info = zpool.info.clone();
                })
                .or_insert_with(|| zpool);
            is_new
        };

        // If we hadn't previously been handling this zpool, hand it off to the
        // worker for management (zone creation).
        if is_new {
            self.new_pools_tx.send(name.to_string()).await.unwrap();
        }
        Ok(())
    }
}

impl Drop for StorageManager {
    fn drop(&mut self) {
        // NOTE: Ideally, with async drop, we'd await completion of the worker
        // somehow.
        //
        // Without that option, we instead opt to simply cancel the worker
        // task to ensure it does not remain alive beyond the StorageManager
        // itself.
        self.task.abort();
    }
}
