//! Backend-owned retention of uploaded and generated frame resources.

use std::collections::BTreeMap;

use mmrecode_core::{Error, Result};

use crate::{FrameDescriptor, FrameHandle, FrameResidency, FrameResourceKey};

/// Result of retaining one semantic frame resource.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum DeviceResourceStatus {
    /// A backend resource was created and inserted.
    Inserted,
    /// The existing backend resource was reused.
    Reused,
}

/// Observable device-cache state and lifetime counters.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DeviceResourceCacheStats {
    /// Number of retained backend resources.
    pub resources: usize,
    /// Backend-estimated bytes retained by the cache.
    pub retained_bytes: usize,
    /// Configured upper bound for retained bytes.
    pub byte_budget: usize,
    /// Number of successful cache reuses.
    pub reuses: u64,
    /// Number of resources created and inserted.
    pub insertions: u64,
    /// Number of resources automatically released for capacity or idleness.
    pub evictions: u64,
    /// Current logical frame generation.
    pub generation: u64,
}

#[derive(Debug)]
struct Entry<Resource> {
    descriptor: FrameDescriptor,
    resource: Resource,
    estimated_bytes: usize,
    last_used_generation: u64,
}

/// Bounded least-recently-used storage owned by a concrete device backend.
///
/// `Resource` can be a wgpu texture/view bundle, a Vulkan image, or a test double. Semantic
/// [`FrameResourceKey`] values remain independent of that backend type. Entries touched in the
/// current generation are protected from automatic eviction so resources required by one graph
/// cannot disappear while its passes execute.
#[derive(Debug)]
pub struct DeviceResourceCache<Resource> {
    backend: String,
    byte_budget: usize,
    retained_bytes: usize,
    generation: u64,
    reuses: u64,
    insertions: u64,
    evictions: u64,
    entries: BTreeMap<FrameResourceKey, Entry<Resource>>,
}

impl<Resource> DeviceResourceCache<Resource> {
    /// Create an empty cache for one named device backend.
    ///
    /// # Errors
    ///
    /// Returns an error when `backend` is empty or `byte_budget` is zero.
    pub fn new(backend: impl Into<String>, byte_budget: usize) -> Result<Self> {
        let backend = backend.into();
        if backend.trim().is_empty() {
            return Err(Error::InvalidData(
                "device resource cache backend name cannot be empty".into(),
            ));
        }
        if byte_budget == 0 {
            return Err(Error::InvalidData(
                "device resource cache byte budget must be positive".into(),
            ));
        }
        Ok(Self {
            backend,
            byte_budget,
            retained_bytes: 0,
            generation: 0,
            reuses: 0,
            insertions: 0,
            evictions: 0,
            entries: BTreeMap::new(),
        })
    }

    /// Backend identity written into device-resident frame handles.
    #[must_use]
    pub fn backend(&self) -> &str {
        &self.backend
    }

    /// Advance the logical frame generation.
    ///
    /// Resources retained or fetched after this call are protected until the next generation.
    pub fn begin_frame(&mut self) {
        self.generation = self.generation.saturating_add(1);
    }

    /// Return a handle with identical semantics and residency assigned to this backend.
    #[must_use]
    pub fn device_handle(&self, handle: &FrameHandle) -> FrameHandle {
        FrameHandle {
            key: handle.key,
            descriptor: handle.descriptor.clone(),
            residency: FrameResidency::Device {
                backend: self.backend.clone(),
            },
        }
    }

    /// Reuse a retained resource or create and retain it once.
    ///
    /// `estimated_bytes` is backend-defined physical storage, including auxiliary views or planes.
    /// The creation closure is never called on a cache hit. If the new resource cannot fit without
    /// evicting something touched in the current generation, it is dropped and the cache is left
    /// within budget.
    ///
    /// # Errors
    ///
    /// Returns an error for a foreign device handle, a stable-key descriptor collision, a zero or
    /// over-budget size, resource creation failure, or insufficient evictable capacity.
    pub fn retain_with<Create>(
        &mut self,
        handle: &FrameHandle,
        estimated_bytes: usize,
        create: Create,
    ) -> Result<(&Resource, DeviceResourceStatus)>
    where
        Create: FnOnce() -> Result<Resource>,
    {
        self.validate_residency(handle)?;
        if self.entries.contains_key(&handle.key) {
            return self.reuse(handle);
        }
        if estimated_bytes == 0 || estimated_bytes > self.byte_budget {
            return Err(Error::InvalidData(format!(
                "device resource {:?} requires {estimated_bytes} bytes with a {} byte budget",
                handle.key, self.byte_budget
            )));
        }
        let resource = create()?;
        self.evict_for(estimated_bytes)?;
        self.entries.insert(
            handle.key,
            Entry {
                descriptor: handle.descriptor.clone(),
                resource,
                estimated_bytes,
                last_used_generation: self.generation,
            },
        );
        self.retained_bytes += estimated_bytes;
        self.insertions = self.insertions.saturating_add(1);
        let entry = self
            .entries
            .get(&handle.key)
            .ok_or_else(|| Error::InvalidState("inserted device resource disappeared".into()))?;
        Ok((&entry.resource, DeviceResourceStatus::Inserted))
    }

    /// Fetch and mark a resource as used in the current generation.
    ///
    /// # Errors
    ///
    /// Returns an error for a foreign device handle or stable-key descriptor collision.
    pub fn get(&mut self, handle: &FrameHandle) -> Result<Option<&Resource>> {
        self.validate_residency(handle)?;
        let Some(entry) = self.entries.get_mut(&handle.key) else {
            return Ok(None);
        };
        if entry.descriptor != handle.descriptor {
            return Err(descriptor_collision(handle.key));
        }
        entry.last_used_generation = self.generation;
        self.reuses = self.reuses.saturating_add(1);
        Ok(Some(&entry.resource))
    }

    /// Explicitly release one semantic resource.
    pub fn remove(&mut self, key: FrameResourceKey) -> Option<Resource> {
        let entry = self.entries.remove(&key)?;
        self.retained_bytes = self.retained_bytes.saturating_sub(entry.estimated_bytes);
        Some(entry.resource)
    }

    /// Release resources idle for more than `maximum_idle_generations` completed generations.
    ///
    /// Returns the number of released resources. Entries touched in the current generation are
    /// never removed.
    pub fn release_idle(&mut self, maximum_idle_generations: u64) -> usize {
        let before = self.entries.len();
        let generation = self.generation;
        let mut released_bytes = 0_usize;
        self.entries.retain(|_, entry| {
            let idle = generation.saturating_sub(entry.last_used_generation);
            let keep = idle == 0 || idle <= maximum_idle_generations;
            if !keep {
                released_bytes = released_bytes.saturating_add(entry.estimated_bytes);
            }
            keep
        });
        let released = before - self.entries.len();
        self.retained_bytes = self.retained_bytes.saturating_sub(released_bytes);
        self.evictions = self
            .evictions
            .saturating_add(u64::try_from(released).unwrap_or(u64::MAX));
        released
    }

    /// Release every retained backend resource.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.retained_bytes = 0;
    }

    /// Current storage and lifetime counters.
    #[must_use]
    pub fn stats(&self) -> DeviceResourceCacheStats {
        DeviceResourceCacheStats {
            resources: self.entries.len(),
            retained_bytes: self.retained_bytes,
            byte_budget: self.byte_budget,
            reuses: self.reuses,
            insertions: self.insertions,
            evictions: self.evictions,
            generation: self.generation,
        }
    }

    fn validate_residency(&self, handle: &FrameHandle) -> Result<()> {
        if let FrameResidency::Device { backend } = &handle.residency
            && backend != &self.backend
        {
            return Err(Error::InvalidState(format!(
                "device resource {:?} belongs to backend '{backend}', not '{}'",
                handle.key, self.backend
            )));
        }
        Ok(())
    }

    fn reuse(&mut self, handle: &FrameHandle) -> Result<(&Resource, DeviceResourceStatus)> {
        let entry = self
            .entries
            .get_mut(&handle.key)
            .expect("device resource existence was checked");
        if entry.descriptor != handle.descriptor {
            return Err(descriptor_collision(handle.key));
        }
        entry.last_used_generation = self.generation;
        self.reuses = self.reuses.saturating_add(1);
        Ok((&entry.resource, DeviceResourceStatus::Reused))
    }

    fn evict_for(&mut self, additional_bytes: usize) -> Result<()> {
        while self.retained_bytes.saturating_add(additional_bytes) > self.byte_budget {
            let candidate = self
                .entries
                .iter()
                .filter(|(_, entry)| entry.last_used_generation < self.generation)
                .min_by_key(|(key, entry)| (entry.last_used_generation, **key))
                .map(|(key, _)| *key)
                .ok_or_else(|| {
                    Error::InvalidState(format!(
                        "device resource budget cannot fit {additional_bytes} more bytes without \
                         releasing a resource used in generation {}",
                        self.generation
                    ))
                })?;
            let entry = self
                .entries
                .remove(&candidate)
                .expect("eviction candidate exists");
            self.retained_bytes = self.retained_bytes.saturating_sub(entry.estimated_bytes);
            self.evictions = self.evictions.saturating_add(1);
        }
        Ok(())
    }
}

fn descriptor_collision(key: FrameResourceKey) -> Error {
    Error::InvalidState(format!(
        "stable device resource key {key:?} was reused with a different descriptor"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FrameDescriptor, FrameResourceNamespace};

    fn handle(owner: u64) -> FrameHandle {
        FrameHandle {
            key: FrameResourceKey {
                namespace: FrameResourceNamespace::MmfxCanvas,
                owner,
                revision: 1,
                local_frame: -1,
                width: 2,
                height: 2,
                variant: 0,
            },
            descriptor: FrameDescriptor::rgba8(2, 2),
            residency: FrameResidency::Cpu,
        }
    }

    #[test]
    fn stable_key_reuses_one_backend_resource() {
        let mut cache = DeviceResourceCache::new("test-gpu", 64).unwrap();
        let frame = handle(1);
        let (resource, status) = cache
            .retain_with(&frame, 16, || Ok(String::from("texture")))
            .unwrap();
        assert_eq!(resource, "texture");
        assert_eq!(status, DeviceResourceStatus::Inserted);
        let (_, status) = cache
            .retain_with(&frame, 16, || panic!("cache hit must not upload again"))
            .unwrap();
        assert_eq!(status, DeviceResourceStatus::Reused);
        assert_eq!(cache.stats().resources, 1);
        assert_eq!(cache.stats().reuses, 1);
        assert!(matches!(
            cache.device_handle(&frame).residency,
            FrameResidency::Device { ref backend } if backend == "test-gpu"
        ));
    }

    #[test]
    fn evicts_oldest_resource_but_protects_current_graph_generation() {
        let mut cache = DeviceResourceCache::new("test-gpu", 32).unwrap();
        let first = handle(1);
        let second = handle(2);
        let third = handle(3);
        cache.retain_with(&first, 16, || Ok(1)).unwrap();
        cache.retain_with(&second, 16, || Ok(2)).unwrap();
        cache.begin_frame();
        assert_eq!(cache.get(&second).unwrap(), Some(&2));
        cache.retain_with(&third, 16, || Ok(3)).unwrap();
        assert!(cache.get(&first).unwrap().is_none());
        assert_eq!(cache.get(&second).unwrap(), Some(&2));
        assert_eq!(cache.get(&third).unwrap(), Some(&3));
        assert_eq!(cache.stats().evictions, 1);
    }

    #[test]
    fn refuses_to_evict_a_resource_used_by_the_current_graph() {
        let mut cache = DeviceResourceCache::new("test-gpu", 16).unwrap();
        cache.retain_with(&handle(1), 16, || Ok(1)).unwrap();
        let error = cache.retain_with(&handle(2), 16, || Ok(2)).unwrap_err();
        assert!(error.to_string().contains("used in generation"));
        assert_eq!(cache.stats().resources, 1);
    }

    #[test]
    fn rejects_descriptor_collisions_and_foreign_device_handles() {
        let mut cache = DeviceResourceCache::new("test-gpu", 64).unwrap();
        let original = handle(1);
        cache.retain_with(&original, 16, || Ok(1)).unwrap();
        let mut collision = original.clone();
        collision.descriptor = FrameDescriptor::rgba8(4, 4);
        assert!(cache.get(&collision).is_err());
        let mut foreign = handle(2);
        foreign.residency = FrameResidency::Device {
            backend: "another-gpu".into(),
        };
        assert!(cache.retain_with(&foreign, 16, || Ok(2)).is_err());
    }

    #[test]
    fn releases_only_idle_generations() {
        let mut cache = DeviceResourceCache::new("test-gpu", 64).unwrap();
        let first = handle(1);
        let second = handle(2);
        cache.retain_with(&first, 16, || Ok(1)).unwrap();
        cache.begin_frame();
        cache.retain_with(&second, 16, || Ok(2)).unwrap();
        cache.begin_frame();
        assert_eq!(cache.get(&second).unwrap(), Some(&2));
        assert_eq!(cache.release_idle(1), 1);
        assert!(cache.get(&first).unwrap().is_none());
        assert_eq!(cache.get(&second).unwrap(), Some(&2));
    }
}
