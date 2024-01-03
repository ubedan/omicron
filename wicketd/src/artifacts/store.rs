// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use super::artifacts_with_plan::ArtifactsWithPlan;
use super::ExtractedArtifactDataHandle;
use super::UpdatePlan;
use crate::http_entrypoints::InstallableArtifacts;
use dropshot::HttpError;
use omicron_common::api::external::SemverVersion;
use omicron_common::update::ArtifactHashId;
use slog::Logger;
use std::io;
use std::sync::Arc;
use std::sync::Mutex;

/// The artifact store for wicketd.
///
/// This can be cheaply cloned, and is intended to be shared across the parts of artifactd that
/// upload artifacts and the parts that fetch them.
#[derive(Clone, Debug)]
pub struct WicketdArtifactStore {
    log: Logger,
    // NOTE: this is a `std::sync::Mutex` rather than a `tokio::sync::Mutex`
    // because the critical sections are extremely small.
    artifacts_with_plan: Arc<Mutex<Option<ArtifactsWithPlan>>>,
}

impl WicketdArtifactStore {
    pub(crate) fn new(log: &Logger) -> Self {
        let log = log.new(slog::o!("component" => "wicketd artifact store"));
        Self { log, artifacts_with_plan: Default::default() }
    }

    pub(crate) async fn put_repository<T>(
        &self,
        data: T,
    ) -> Result<(), HttpError>
    where
        T: io::Read + io::Seek + Send + 'static,
    {
        slog::debug!(self.log, "adding repository");

        let log = self.log.clone();
        let new_artifacts = ArtifactsWithPlan::from_zip(data, &log)
            .await
            .map_err(|error| error.to_http_error())?;
        self.replace(new_artifacts);

        Ok(())
    }

    pub(crate) fn system_version_and_artifact_ids(
        &self,
    ) -> Option<(SemverVersion, Vec<InstallableArtifacts>)> {
        let artifacts = self.artifacts_with_plan.lock().unwrap();
        let artifacts = artifacts.as_ref()?;
        let system_version = artifacts.plan().system_version.clone();
        let artifact_ids = artifacts
            .by_id()
            .iter()
            .map(|(k, v)| InstallableArtifacts {
                artifact_id: k.clone(),
                installable: v.clone(),
            })
            .collect();
        Some((system_version, artifact_ids))
    }

    /// Obtain the current plan.
    ///
    /// Exposed for testing.
    pub fn current_plan(&self) -> Option<UpdatePlan> {
        // We expect this hashmap to be relatively small (order ~10), and
        // cloning both ArtifactIds and ExtractedArtifactDataHandles are cheap.
        self.artifacts_with_plan
            .lock()
            .unwrap()
            .as_ref()
            .map(|artifacts| artifacts.plan().clone())
    }

    // ---
    // Helper methods
    // ---

    pub(super) fn get_by_hash(
        &self,
        id: &ArtifactHashId,
    ) -> Option<ExtractedArtifactDataHandle> {
        self.artifacts_with_plan.lock().unwrap().as_ref()?.get_by_hash(id)
    }

    // `pub` to allow use in integration tests.
    pub fn contains_by_hash(&self, id: &ArtifactHashId) -> bool {
        self.get_by_hash(id).is_some()
    }

    /// Replaces the artifact hash map, returning the previous map.
    fn replace(
        &self,
        new_artifacts: ArtifactsWithPlan,
    ) -> Option<ArtifactsWithPlan> {
        let mut artifacts = self.artifacts_with_plan.lock().unwrap();
        std::mem::replace(&mut *artifacts, Some(new_artifacts))
    }
}
