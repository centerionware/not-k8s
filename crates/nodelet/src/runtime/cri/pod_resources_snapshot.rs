use super::*;

impl CriRuntime {
    /// Builds `pod_resources_snapshot()`'s data straight from this
    /// runtime's own CPU/Memory/device-manager state and CRI's live
    /// sandbox/container listing — no new bookkeeping of its own, just a
    /// read-only projection (round 74; found in round 72's re-audit).
    pub(crate) async fn build_pod_resources_snapshot(&self) -> Vec<crate::runtime::PodResourcesEntry> {
        let Ok(sandboxes) = self.list_all_sandboxes().await else {
            return Vec::new();
        };
        let mut out = Vec::with_capacity(sandboxes.len());
        for (namespace, name, sandbox_id) in sandboxes {
            let Ok(containers) = self.list_pod_containers(&sandbox_id).await else { continue };
            let mut entries = Vec::with_capacity(containers.len());
            for c in containers {
                let Some(container_name) = c.labels.get(CTR_NAME_LABEL) else { continue };
                let key = restart_count_key(&sandbox_id, container_name);
                let cpu_ids = self
                    .cpu_manager
                    .as_ref()
                    .and_then(|m| m.assigned(&key))
                    .map(|set| set.into_iter().map(i64::from).collect())
                    .unwrap_or_default();
                let memory = self.memory_manager.as_ref().and_then(|m| m.assigned(&key)).map(|entry| vec![entry]).unwrap_or_default();
                let devices = self.device_allocations.lock().unwrap().get(&key).cloned().unwrap_or_default();
                entries.push(crate::runtime::ContainerResourcesEntry { name: container_name.clone(), cpu_ids, devices, memory });
            }
            out.push(crate::runtime::PodResourcesEntry { namespace, name, containers: entries });
        }
        out
    }

    /// Builds `allocatable_resources()`'s data — the whole
    /// static-policy-managed pool for each resource kind, not just what's
    /// currently free (matching real kubelet's own `GetAllocatableResources`
    /// semantics).
    pub(crate) fn build_allocatable_resources(&self) -> crate::runtime::AllocatableResourcesSnapshot {
        let cpu_ids = self.cpu_manager.as_ref().map(|m| m.allocatable_cpus().into_iter().map(i64::from).collect()).unwrap_or_default();
        let memory =
            self.memory_manager.as_ref().map(|m| m.capacity_per_node().into_iter().collect()).unwrap_or_default();
        let devices = self.device_plugins.all_healthy_device_ids();
        crate::runtime::AllocatableResourcesSnapshot { cpu_ids, devices, memory }
    }
}
