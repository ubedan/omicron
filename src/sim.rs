/*!
 * API simulation of an Oxide rack, used for testing and prototyping.
 */

use async_trait::async_trait;
use futures::lock::Mutex;
use futures::stream::StreamExt;
use std::collections::BTreeMap;

use crate::api_error::ApiError;
use crate::api_model::ApiBackend;
use crate::api_model::ApiCreateResult;
use crate::api_model::ApiListResult;
use crate::api_model::ApiModelProject;
use crate::api_model::ApiModelProjectCreate;

/**
 * SimulatorBuilder is used to initialize and populate a Simulator
 * synchronously, before we've wrapped the guts in a Mutex that can only be
 * locked in an async context.
 */
pub struct SimulatorBuilder {
    projects_by_name: BTreeMap<String, SimProject>
}

impl SimulatorBuilder {
    pub fn new() -> Self
    {
        SimulatorBuilder {
            projects_by_name: BTreeMap::new()
        }
    }

    pub fn project_create(&mut self, project_name: &str)
    {
        let name = project_name.to_string();
        let project = SimProject {
            name: name
        };

        self.projects_by_name.insert(project_name.to_string(), project);
    }

    pub fn build(self)
        -> Simulator
    {
        Simulator {
            projects_by_name: Mutex::new(self.projects_by_name)
        }
    }
}

/**
 * Maintains simulated state of the Oxide rack.  The current implementation is
 * in-memory only.
 */
pub struct Simulator {
    projects_by_name: Mutex<BTreeMap<String, SimProject>>
}

#[async_trait]
impl ApiBackend for Simulator {
    async fn projects_list(&self)
        -> ApiListResult<ApiModelProject>
    {
        /*
         * Assemble the list of projects under the lock, then stream the list
         * asynchronously.  This probably seems a little strange (why is this
         * async to begin with?), but the more realistic backend that we're
         * simulating is likely to be streaming this from a database or the
         * like.
         */
        let projects_by_name = self.projects_by_name.lock().await;
        let projects : Vec<Result<ApiModelProject, ApiError>> = projects_by_name
            .values()
            .map(|sim_project| Ok(ApiModelProject {
                name: sim_project.name.clone()
            })).collect();

        Ok(futures::stream::iter(projects).boxed())
    }

    async fn project_create(&self, new_project: &ApiModelProjectCreate)
        -> ApiCreateResult<ApiModelProject>
    {
        let mut projects_by_name = self.projects_by_name.lock().await;
        if projects_by_name.contains_key(&new_project.name) {
            // XXX better error
            return Err(ApiError {});
        }

        let newname = &new_project.name;
        let project = SimProject { name: newname.clone() };
        projects_by_name.insert(newname.clone(), project);
        Ok(ApiModelProject {
            name: newname.clone()
        })
    }
}

struct SimProject {
    name: String
}
