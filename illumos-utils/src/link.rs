// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! API for allocating and managing data links.

use crate::destructor::{Deletable, Destructor};
use crate::dladm::{
    CreateVnicError, DeleteVnicError, Dladm, VnicSource, VNIC_PREFIX,
    VNIC_PREFIX_BOOTSTRAP, VNIC_PREFIX_CONTROL, VNIC_PREFIX_GUEST,
};
use crate::host::BoxedExecutor;
use omicron_common::api::external::MacAddr;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

/// A shareable wrapper around an atomic counter.
/// May be used to allocate runtime-unique IDs for objects
/// which have naming constraints - such as VNICs.
#[derive(Clone)]
pub struct VnicAllocator<DL: VnicSource + 'static> {
    executor: BoxedExecutor,
    value: Arc<AtomicU64>,
    scope: String,
    data_link: DL,
    // Manages dropped Vnics, and repeatedly attempts to delete them.
    destructor: Destructor<VnicDestruction>,
}

impl<DL: VnicSource + Clone> VnicAllocator<DL> {
    /// Creates a new Vnic name allocator with a particular scope.
    ///
    /// The intent with varying scopes is to create non-overlapping
    /// ranges of Vnic names, for example:
    ///
    /// VnicAllocator::new("Instance")
    /// - oxGuestInstance0
    /// - oxControlInstance0
    ///
    /// VnicAllocator::new("Storage") produces
    /// - oxControlStorage0
    pub fn new<S: AsRef<str>>(
        executor: &BoxedExecutor,
        scope: S,
        data_link: DL,
    ) -> Self {
        Self {
            executor: executor.clone(),
            value: Arc::new(AtomicU64::new(0)),
            scope: scope.as_ref().to_string(),
            data_link,
            destructor: Destructor::new(),
        }
    }

    /// Creates a new NIC, intended for allowing Propolis to communicate
    /// with the control plane.
    pub fn new_control(
        &self,
        mac: Option<MacAddr>,
    ) -> Result<Link, CreateVnicError> {
        let allocator = self.new_superscope("Control");
        let name = allocator.next();
        debug_assert!(name.starts_with(VNIC_PREFIX));
        debug_assert!(name.starts_with(VNIC_PREFIX_CONTROL));
        Dladm::create_vnic(
            &self.executor,
            &self.data_link,
            &name,
            mac,
            None,
            9000,
        )?;
        Ok(Link {
            executor: self.executor.clone(),
            name,
            deleted: false,
            kind: LinkKind::OxideControlVnic,
            destructor: Some(self.destructor.clone()),
        })
    }

    /// Takes ownership of an existing VNIC.
    pub fn wrap_existing<S: AsRef<str>>(
        &self,
        name: S,
    ) -> Result<Link, InvalidLinkKind> {
        match LinkKind::from_name(name.as_ref()) {
            Some(kind) => Ok(Link {
                executor: self.executor.clone(),
                name: name.as_ref().to_owned(),
                deleted: false,
                kind,
                destructor: Some(self.destructor.clone()),
            }),
            None => Err(InvalidLinkKind(name.as_ref().to_owned())),
        }
    }

    fn new_superscope<S: AsRef<str>>(&self, scope: S) -> Self {
        Self {
            executor: self.executor.clone(),
            value: self.value.clone(),
            scope: format!("{}{}", scope.as_ref(), self.scope),
            data_link: self.data_link.clone(),
            destructor: self.destructor.clone(),
        }
    }

    pub fn new_bootstrap(&self) -> Result<Link, CreateVnicError> {
        let name = self.next();
        Dladm::create_vnic(
            &self.executor,
            &self.data_link,
            &name,
            None,
            None,
            1500,
        )?;
        Ok(Link {
            executor: self.executor.clone(),
            name,
            deleted: false,
            kind: LinkKind::OxideBootstrapVnic,
            destructor: Some(self.destructor.clone()),
        })
    }

    /// Allocates a new VNIC name, which should be unique within the
    /// scope of this allocator.
    fn next(&self) -> String {
        format!("{}{}{}", VNIC_PREFIX, self.scope, self.next_id())
    }

    fn next_id(&self) -> u64 {
        self.value.fetch_add(1, Ordering::SeqCst)
    }
}

/// Represents the kind of a Link, such as whether it's for guest networking or
/// communicating with Oxide services.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LinkKind {
    Physical,
    OxideControlVnic,
    GuestVnic,
    OxideBootstrapVnic,
}

impl LinkKind {
    /// Infer the kind from a VNIC's name, if this one the sled agent can
    /// manage, and `None` otherwise.
    pub fn from_name(name: &str) -> Option<Self> {
        if name.starts_with(VNIC_PREFIX) {
            Some(LinkKind::OxideControlVnic)
        } else if name.starts_with(VNIC_PREFIX_GUEST) {
            Some(LinkKind::GuestVnic)
        } else if name.starts_with(VNIC_PREFIX_BOOTSTRAP) {
            Some(LinkKind::OxideBootstrapVnic)
        } else {
            None
        }
    }
}

#[derive(thiserror::Error, Debug)]
#[error("VNIC with name '{0}' is not valid for sled agent management")]
pub struct InvalidLinkKind(String);

/// Represents an allocated VNIC on the system.
/// The VNIC is de-allocated when it goes out of scope.
///
/// Note that the "ownership" of the VNIC is based on convention;
/// another process in the global zone could also modify / destroy
/// the VNIC while this object is alive.
pub struct Link {
    executor: BoxedExecutor,
    name: String,
    deleted: bool,
    kind: LinkKind,
    destructor: Option<Destructor<VnicDestruction>>,
}

impl std::fmt::Debug for Link {
    fn fmt(&self, fmt: &mut std::fmt::Formatter) -> std::fmt::Result {
        fmt.debug_struct("Link")
            .field("name", &self.name)
            .field("deleted", &self.deleted)
            .field("kind", &self.kind)
            .finish()
    }
}

impl Link {
    /// Wraps a physical nic in a Link structure.
    ///
    /// It is the caller's responsibility to ensure this is a physical link.
    pub fn wrap_physical<S: AsRef<str>>(
        executor: &BoxedExecutor,
        name: S,
    ) -> Self {
        Link {
            executor: executor.clone(),
            name: name.as_ref().to_owned(),
            deleted: false,
            kind: LinkKind::Physical,
            destructor: None,
        }
    }

    /// Deletes a NIC (if it has not already been deleted).
    pub fn delete(&mut self) -> Result<(), DeleteVnicError> {
        if self.deleted || self.kind == LinkKind::Physical {
            Ok(())
        } else {
            Dladm::delete_vnic(&self.executor, &self.name)?;
            self.deleted = true;
            Ok(())
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn kind(&self) -> LinkKind {
        self.kind
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        if let Some(destructor) = self.destructor.take() {
            destructor.enqueue_destroy(VnicDestruction {
                executor: self.executor.clone(),
                name: self.name.clone(),
            });
        }
    }
}

// Represents the request to destroy a VNIC
struct VnicDestruction {
    name: String,
    executor: BoxedExecutor,
}

#[async_trait::async_trait]
impl Deletable for VnicDestruction {
    async fn delete(&self) -> Result<(), anyhow::Error> {
        Dladm::delete_vnic(&self.executor, &self.name)?;
        Ok(())
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::dladm::Etherstub;
    use crate::host::fake::FakeExecutorBuilder;
    use omicron_test_utils::dev;

    #[tokio::test]
    async fn test_allocate() {
        let logctx = dev::test_setup_log("test_allocate");
        let executor = FakeExecutorBuilder::new(logctx.log.clone()).build();
        let allocator = VnicAllocator::new(
            &executor.as_executor(),
            "Foo",
            Etherstub("mystub".to_string()),
        );
        assert_eq!("oxFoo0", allocator.next());
        assert_eq!("oxFoo1", allocator.next());
        assert_eq!("oxFoo2", allocator.next());
        logctx.cleanup_successful();
    }

    #[tokio::test]
    async fn test_allocate_within_scopes() {
        let logctx = dev::test_setup_log("test_allocate_within_scopes");
        let executor = FakeExecutorBuilder::new(logctx.log.clone()).build();
        let allocator = VnicAllocator::new(
            &executor.as_executor(),
            "Foo",
            Etherstub("mystub".to_string()),
        );
        assert_eq!("oxFoo0", allocator.next());
        let allocator = allocator.new_superscope("Baz");
        assert_eq!("oxBazFoo1", allocator.next());
        logctx.cleanup_successful();
    }
}
