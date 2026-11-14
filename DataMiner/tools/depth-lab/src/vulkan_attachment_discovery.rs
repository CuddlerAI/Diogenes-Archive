//! Fail-closed contract skeleton for Vulkan depth-attachment discovery.
//!
//! There is deliberately no Vulkan layer runtime in this module. The only admitted backend state
//! is [`VULKAN_DISCOVERY_BACKEND_STATE`], and every report remains blocked by
//! [`VulkanDiscoveryBlocker::RuntimeBackendUnavailable`]. Synthetic observations exercise the
//! contract and object-generation machines; they are never runtime evidence and can never author
//! a depth profile, camera binding, or recording-ready claim.
//!
//! The registries are intentionally plain mutable state, not internally synchronized. A future
//! layer must serialize all cross-thread callbacks into one bounded total-order ledger before
//! calling them. Lock contention, queue loss, a thread-local merge, or any partially observed
//! dispatch chain must increment a durable blocker instead of guessing an order.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

pub const VULKAN_ATTACHMENT_DISCOVERY_CONFIG_SCHEMA_V1: &str =
    "vulkan-attachment-discovery-config/v1";
pub const VULKAN_ATTACHMENT_DISCOVERY_REPORT_SCHEMA_V1: &str =
    "vulkan-attachment-discovery-contract-report/v1";
pub const VULKAN_DISCOVERY_BACKEND_STATE: &str = "unavailable/discovery_contract";

pub const MAX_VULKAN_DISCOVERY_FRAMES: u32 = 1_200;
pub const MAX_VULKAN_DISCOVERY_CANDIDATES: u32 = 256;
pub const MAX_VULKAN_DISCOVERY_CANDIDATES_PER_FRAME: u32 = 64;
pub const MAX_VULKAN_LIVE_IMAGES: u32 = 16_384;
pub const MAX_VULKAN_TRACKED_IMAGE_HANDLES: u32 = 65_536;
pub const MAX_VULKAN_LIVE_IMAGE_VIEWS: u32 = 32_768;
pub const MAX_VULKAN_TRACKED_IMAGE_VIEW_HANDLES: u32 = 131_072;
pub const MAX_VULKAN_LIVE_COMMAND_POOLS: u32 = 4_096;
pub const MAX_VULKAN_LIVE_COMMAND_BUFFERS: u32 = 65_536;
pub const MAX_VULKAN_TRACKED_COMMAND_BUFFER_HANDLES: u32 = 262_144;
pub const MAX_VULKAN_SECONDARIES_PER_PRIMARY: u32 = 4_096;
pub const MAX_VULKAN_SUBMISSIONS_PER_PRESENT: u32 = 4_096;
pub const MAX_VULKAN_TIMELINE_SIGNALS_PER_PRESENT: u32 = 4_096;
pub const MAX_VULKAN_AMBIGUOUS_GROUPS_PER_PRESENT: u32 = 64;
pub const MAX_VULKAN_IDS_PER_AMBIGUOUS_GROUP: u32 = 64;
pub const MAX_VULKAN_LAYER_ENTRYPOINT_CAPABILITIES: usize = 128;

/// A layer that only wraps `vkCmdBeginRenderPass` / `vkCmdBeginRendering` cannot relate recorded
/// commands to a later present. These are the minimum interception families the future runtime
/// must cover before its output may even be considered complete discovery evidence.
///
/// Core and KHR aliases are both required: an application may request either symbol. The list also
/// includes object and command-buffer lifetimes, secondary replay, submit variants, timeline
/// operations, swapchain acquisition/present, subpass changes, clears, and layout transitions.
pub const REQUIRED_VULKAN_DISCOVERY_ENTRYPOINTS: &[&str] = &[
    "vkNegotiateLoaderLayerInterfaceVersion",
    "vkGetInstanceProcAddr",
    "vkGetDeviceProcAddr",
    "vkCreateInstance",
    "vkDestroyInstance",
    "vkCreateDevice",
    "vkDestroyDevice",
    "vkGetDeviceQueue",
    "vkGetDeviceQueue2",
    "vkCreateImage",
    "vkDestroyImage",
    "vkCreateImageView",
    "vkDestroyImageView",
    "vkCreateRenderPass",
    "vkCreateRenderPass2",
    "vkCreateRenderPass2KHR",
    "vkDestroyRenderPass",
    "vkCreateFramebuffer",
    "vkDestroyFramebuffer",
    "vkCreateSemaphore",
    "vkDestroySemaphore",
    "vkGetSemaphoreCounterValue",
    "vkGetSemaphoreCounterValueKHR",
    "vkSignalSemaphore",
    "vkSignalSemaphoreKHR",
    "vkWaitSemaphores",
    "vkWaitSemaphoresKHR",
    "vkCreateCommandPool",
    "vkDestroyCommandPool",
    "vkAllocateCommandBuffers",
    "vkFreeCommandBuffers",
    "vkBeginCommandBuffer",
    "vkEndCommandBuffer",
    "vkResetCommandBuffer",
    "vkResetCommandPool",
    "vkCmdBeginRenderPass",
    "vkCmdBeginRenderPass2",
    "vkCmdBeginRenderPass2KHR",
    "vkCmdNextSubpass",
    "vkCmdNextSubpass2",
    "vkCmdNextSubpass2KHR",
    "vkCmdEndRenderPass",
    "vkCmdEndRenderPass2",
    "vkCmdEndRenderPass2KHR",
    "vkCmdBeginRendering",
    "vkCmdBeginRenderingKHR",
    "vkCmdEndRendering",
    "vkCmdEndRenderingKHR",
    "vkCmdClearAttachments",
    "vkCmdPipelineBarrier",
    "vkCmdPipelineBarrier2",
    "vkCmdPipelineBarrier2KHR",
    "vkCmdExecuteCommands",
    "vkQueueSubmit",
    "vkQueueSubmit2",
    "vkQueueSubmit2KHR",
    "vkQueueBindSparse",
    "vkCreateSwapchainKHR",
    "vkDestroySwapchainKHR",
    "vkGetSwapchainImagesKHR",
    "vkAcquireNextImageKHR",
    "vkAcquireNextImage2KHR",
    "vkQueuePresentKHR",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VulkanDiscoveryLimits {
    pub max_frames: u32,
    pub max_candidates: u32,
    pub max_candidates_per_frame: u32,
    pub max_live_images: u32,
    pub max_tracked_image_handles: u32,
    pub max_live_image_views: u32,
    pub max_tracked_image_view_handles: u32,
    pub max_live_command_pools: u32,
    pub max_live_command_buffers: u32,
    pub max_tracked_command_buffer_handles: u32,
    pub max_secondaries_per_primary: u32,
    pub max_submissions_per_present: u32,
    pub max_timeline_signals_per_present: u32,
    pub max_ambiguous_groups_per_present: u32,
    pub max_ids_per_ambiguous_group: u32,
}

impl Default for VulkanDiscoveryLimits {
    fn default() -> Self {
        Self {
            max_frames: 600,
            max_candidates: 128,
            max_candidates_per_frame: 32,
            max_live_images: 4_096,
            max_tracked_image_handles: 16_384,
            max_live_image_views: 8_192,
            max_tracked_image_view_handles: 32_768,
            max_live_command_pools: 512,
            max_live_command_buffers: 16_384,
            max_tracked_command_buffer_handles: 65_536,
            max_secondaries_per_primary: 1_024,
            max_submissions_per_present: 1_024,
            max_timeline_signals_per_present: 1_024,
            max_ambiguous_groups_per_present: 32,
            max_ids_per_ambiguous_group: 32,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VulkanAttachmentDiscoveryConfig {
    pub schema: String,
    /// Must remain `unavailable/discovery_contract` until a real loader layer is implemented and
    /// independently exercised against a Vulkan runtime.
    pub backend_state: String,
    pub discovery_id: String,
    pub game_id: String,
    pub app_id: String,
    pub build_id: String,
    pub executable_basename: String,
    pub executable_sha256: String,
    pub process_launch_id: String,
    pub process_start_filetime: u64,
    pub vulkan_loader_sha256: String,
    pub layer_manifest_sha256: String,
    pub layer_library_sha256: String,
    pub runtime_gate_report_sha256: String,
    pub rights_evidence_sha256: String,
    #[serde(default)]
    pub limits: VulkanDiscoveryLimits,
}

impl VulkanAttachmentDiscoveryConfig {
    pub fn validate(&self) -> Result<(), String> {
        if self.schema != VULKAN_ATTACHMENT_DISCOVERY_CONFIG_SCHEMA_V1 {
            return Err("Vulkan attachment discovery config schema is unsupported".into());
        }
        if self.backend_state != VULKAN_DISCOVERY_BACKEND_STATE {
            return Err(
                "Vulkan runtime backend is not implemented; only discovery_contract is admitted"
                    .into(),
            );
        }
        for (label, value) in [
            ("discovery_id", self.discovery_id.as_str()),
            ("game_id", self.game_id.as_str()),
            ("app_id", self.app_id.as_str()),
            ("build_id", self.build_id.as_str()),
            ("process_launch_id", self.process_launch_id.as_str()),
        ] {
            validate_token(label, value)?;
        }
        if self.executable_basename.is_empty()
            || self.executable_basename.len() > 260
            || self.executable_basename.contains(['/', '\\'])
            || !self
                .executable_basename
                .to_ascii_lowercase()
                .ends_with(".exe")
        {
            return Err("Vulkan exact-build executable_basename is invalid".into());
        }
        if self.process_start_filetime == 0 {
            return Err("Vulkan runtime gate process_start_filetime must be non-zero".into());
        }
        for (label, digest) in [
            ("executable", self.executable_sha256.as_str()),
            ("Vulkan loader", self.vulkan_loader_sha256.as_str()),
            ("layer manifest", self.layer_manifest_sha256.as_str()),
            ("layer library", self.layer_library_sha256.as_str()),
            (
                "runtime gate report",
                self.runtime_gate_report_sha256.as_str(),
            ),
            ("rights evidence", self.rights_evidence_sha256.as_str()),
        ] {
            if !crate::camera_binding::is_sha256_digest(digest) {
                return Err(format!("Vulkan {label} hash is not canonical SHA-256"));
            }
        }
        validate_limits(self.limits)?;
        Ok(())
    }
}

fn validate_limits(limits: VulkanDiscoveryLimits) -> Result<(), String> {
    let within = |value: u32, maximum: u32| (1..=maximum).contains(&value);
    if !within(limits.max_frames, MAX_VULKAN_DISCOVERY_FRAMES)
        || !within(limits.max_candidates, MAX_VULKAN_DISCOVERY_CANDIDATES)
        || !within(
            limits.max_candidates_per_frame,
            MAX_VULKAN_DISCOVERY_CANDIDATES_PER_FRAME,
        )
        || !within(limits.max_live_images, MAX_VULKAN_LIVE_IMAGES)
        || !within(
            limits.max_tracked_image_handles,
            MAX_VULKAN_TRACKED_IMAGE_HANDLES,
        )
        || !within(limits.max_live_image_views, MAX_VULKAN_LIVE_IMAGE_VIEWS)
        || !within(
            limits.max_tracked_image_view_handles,
            MAX_VULKAN_TRACKED_IMAGE_VIEW_HANDLES,
        )
        || !within(limits.max_live_command_pools, MAX_VULKAN_LIVE_COMMAND_POOLS)
        || !within(
            limits.max_live_command_buffers,
            MAX_VULKAN_LIVE_COMMAND_BUFFERS,
        )
        || !within(
            limits.max_tracked_command_buffer_handles,
            MAX_VULKAN_TRACKED_COMMAND_BUFFER_HANDLES,
        )
        || !within(
            limits.max_secondaries_per_primary,
            MAX_VULKAN_SECONDARIES_PER_PRIMARY,
        )
        || !within(
            limits.max_submissions_per_present,
            MAX_VULKAN_SUBMISSIONS_PER_PRESENT,
        )
        || !within(
            limits.max_timeline_signals_per_present,
            MAX_VULKAN_TIMELINE_SIGNALS_PER_PRESENT,
        )
        || !within(
            limits.max_ambiguous_groups_per_present,
            MAX_VULKAN_AMBIGUOUS_GROUPS_PER_PRESENT,
        )
        || !within(
            limits.max_ids_per_ambiguous_group,
            MAX_VULKAN_IDS_PER_AMBIGUOUS_GROUP,
        )
        || limits.max_live_images > limits.max_tracked_image_handles
        || limits.max_live_image_views > limits.max_tracked_image_view_handles
        || limits.max_live_command_buffers > limits.max_tracked_command_buffer_handles
        || limits.max_candidates_per_frame > limits.max_candidates
        || limits.max_secondaries_per_primary > limits.max_live_command_buffers
        || limits.max_ids_per_ambiguous_group > limits.max_candidates_per_frame
    {
        return Err("Vulkan attachment discovery limits are outside admitted bounds".into());
    }
    Ok(())
}

fn validate_token(label: &str, value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(format!(
            "Vulkan {label} must contain only bounded ASCII letters, digits, -, _ or ."
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VulkanRenderingPath {
    RenderPass,
    DynamicRendering,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VulkanImageDescriptor {
    /// Raw `VkFormat`; zero (`VK_FORMAT_UNDEFINED`) is never a depth candidate.
    pub format: u32,
    pub width: u32,
    pub height: u32,
    pub depth: u32,
    pub mip_levels: u32,
    pub array_layers: u32,
    pub samples: u32,
    pub usage: u32,
}

impl VulkanImageDescriptor {
    fn validate(&self) -> Result<(), String> {
        if self.format == 0
            || self.width == 0
            || self.height == 0
            || self.depth == 0
            || self.mip_levels == 0
            || self.array_layers == 0
            || !matches!(self.samples, 1 | 2 | 4 | 8 | 16 | 32 | 64)
        {
            return Err("Vulkan image descriptor is incomplete or invalid".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VulkanImageViewDescriptor {
    /// Raw `VkFormat`; retained numerically so an older reader never guesses a future label.
    pub format: u32,
    pub view_type: u32,
    pub aspect_mask: u32,
    pub base_mip_level: u32,
    pub level_count: u32,
    pub base_array_layer: u32,
    pub layer_count: u32,
}

impl VulkanImageViewDescriptor {
    fn validate_for(&self, image: &VulkanImageDescriptor) -> Result<(), String> {
        const VK_IMAGE_ASPECT_DEPTH_BIT: u32 = 0x0000_0002;
        if self.format == 0
            || self.aspect_mask & VK_IMAGE_ASPECT_DEPTH_BIT == 0
            || self.level_count == 0
            || self.layer_count == 0
            || self
                .base_mip_level
                .checked_add(self.level_count)
                .is_none_or(|end| end > image.mip_levels)
            || self
                .base_array_layer
                .checked_add(self.layer_count)
                .is_none_or(|end| end > image.array_layers)
        {
            return Err("Vulkan depth image-view descriptor is invalid for its image".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VulkanAttachmentIdentity {
    pub image_handle: u64,
    pub image_generation: u64,
    pub view_handle: u64,
    pub view_generation: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VulkanAttachmentDescriptor {
    pub image: VulkanImageDescriptor,
    pub view: VulkanImageViewDescriptor,
    pub rendering_path: VulkanRenderingPath,
}

impl VulkanAttachmentDescriptor {
    fn validate(&self) -> Result<(), String> {
        self.image.validate()?;
        self.view.validate_for(&self.image)
    }
}

#[derive(Debug, Clone)]
struct LiveImage {
    generation: u64,
    descriptor: VulkanImageDescriptor,
}

#[derive(Debug, Clone)]
struct LiveView {
    generation: u64,
    image_handle: u64,
    image_generation: u64,
    descriptor: VulkanImageViewDescriptor,
}

/// Handle-reuse-safe image/view registry for the future layer. A raw Vulkan handle is never an
/// identity by itself: each successful create after destroy advances its generation.
#[derive(Debug)]
pub struct VulkanAttachmentRegistry {
    limits: VulkanDiscoveryLimits,
    images: BTreeMap<u64, LiveImage>,
    views: BTreeMap<u64, LiveView>,
    last_image_generation: BTreeMap<u64, u64>,
    last_view_generation: BTreeMap<u64, u64>,
}

impl Default for VulkanAttachmentRegistry {
    fn default() -> Self {
        Self::with_limits(VulkanDiscoveryLimits::default())
            .expect("default Vulkan discovery limits are valid")
    }
}

impl VulkanAttachmentRegistry {
    pub fn with_limits(limits: VulkanDiscoveryLimits) -> Result<Self, String> {
        validate_limits(limits)?;
        Ok(Self {
            limits,
            images: BTreeMap::new(),
            views: BTreeMap::new(),
            last_image_generation: BTreeMap::new(),
            last_view_generation: BTreeMap::new(),
        })
    }

    pub fn create_image(
        &mut self,
        handle: u64,
        descriptor: VulkanImageDescriptor,
    ) -> Result<u64, String> {
        if handle == 0 || self.images.contains_key(&handle) {
            return Err("Vulkan image create reused a live or null handle".into());
        }
        if self.images.len() >= self.limits.max_live_images as usize
            || (!self.last_image_generation.contains_key(&handle)
                && self.last_image_generation.len()
                    >= self.limits.max_tracked_image_handles as usize)
        {
            return Err("Vulkan image registry bound exceeded".into());
        }
        descriptor.validate()?;
        let generation = next_generation(&mut self.last_image_generation, handle)?;
        self.images.insert(
            handle,
            LiveImage {
                generation,
                descriptor,
            },
        );
        Ok(generation)
    }

    pub fn destroy_image(&mut self, handle: u64) -> Result<(), String> {
        if self.views.values().any(|view| view.image_handle == handle) {
            return Err(
                "Vulkan image was destroyed while a tracked image view remained live".into(),
            );
        }
        self.images
            .remove(&handle)
            .map(|_| ())
            .ok_or_else(|| "Vulkan image destroy referenced an unknown/dead generation".into())
    }

    pub fn create_view(
        &mut self,
        view_handle: u64,
        image_handle: u64,
        descriptor: VulkanImageViewDescriptor,
    ) -> Result<VulkanAttachmentIdentity, String> {
        if view_handle == 0 || self.views.contains_key(&view_handle) {
            return Err("Vulkan image-view create reused a live or null handle".into());
        }
        if self.views.len() >= self.limits.max_live_image_views as usize
            || (!self.last_view_generation.contains_key(&view_handle)
                && self.last_view_generation.len()
                    >= self.limits.max_tracked_image_view_handles as usize)
        {
            return Err("Vulkan image-view registry bound exceeded".into());
        }
        let image = self
            .images
            .get(&image_handle)
            .ok_or("Vulkan image-view create referenced an unknown/dead image generation")?;
        descriptor.validate_for(&image.descriptor)?;
        let generation = next_generation(&mut self.last_view_generation, view_handle)?;
        let identity = VulkanAttachmentIdentity {
            image_handle,
            image_generation: image.generation,
            view_handle,
            view_generation: generation,
        };
        self.views.insert(
            view_handle,
            LiveView {
                generation,
                image_handle,
                image_generation: image.generation,
                descriptor,
            },
        );
        Ok(identity)
    }

    pub fn destroy_view(&mut self, view_handle: u64) -> Result<(), String> {
        self.views
            .remove(&view_handle)
            .map(|_| ())
            .ok_or_else(|| "Vulkan image-view destroy referenced an unknown/dead generation".into())
    }

    pub fn resolve(
        &self,
        identity: VulkanAttachmentIdentity,
        rendering_path: VulkanRenderingPath,
    ) -> Result<VulkanAttachmentDescriptor, String> {
        let image = self
            .images
            .get(&identity.image_handle)
            .ok_or("Vulkan attachment image generation is no longer live")?;
        let view = self
            .views
            .get(&identity.view_handle)
            .ok_or("Vulkan attachment view generation is no longer live")?;
        if image.generation != identity.image_generation
            || view.generation != identity.view_generation
            || view.image_handle != identity.image_handle
            || view.image_generation != identity.image_generation
        {
            return Err("Vulkan attachment handle generation drifted or was reused".into());
        }
        Ok(VulkanAttachmentDescriptor {
            image: image.descriptor.clone(),
            view: view.descriptor.clone(),
            rendering_path,
        })
    }
}

fn next_generation(generations: &mut BTreeMap<u64, u64>, handle: u64) -> Result<u64, String> {
    let next = generations
        .get(&handle)
        .copied()
        .unwrap_or(0)
        .checked_add(1)
        .ok_or("Vulkan object generation overflow")?;
    generations.insert(handle, next);
    Ok(next)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VulkanCommandBufferLevel {
    Primary,
    Secondary,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VulkanCommandBufferIdentity {
    pub handle: u64,
    pub allocation_generation: u64,
    pub recording_generation: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommandBufferState {
    Initial,
    Recording,
    Executable,
}

#[derive(Debug, Clone)]
struct CommandBufferRecord {
    allocation_generation: u64,
    recording_generation: u64,
    pool_handle: u64,
    level: VulkanCommandBufferLevel,
    state: CommandBufferState,
    secondaries: Vec<VulkanCommandBufferIdentity>,
}

/// Pure command-buffer generation machine. It makes reset/free and secondary replay explicit, so
/// a future layer cannot associate a primary submission with whichever recording happens to occupy
/// the same raw handle later.
#[derive(Debug)]
pub struct VulkanCommandBufferRegistry {
    limits: VulkanDiscoveryLimits,
    buffers: BTreeMap<u64, CommandBufferRecord>,
    last_allocation_generation: BTreeMap<u64, u64>,
    live_pools: BTreeSet<u64>,
}

impl Default for VulkanCommandBufferRegistry {
    fn default() -> Self {
        Self::with_limits(VulkanDiscoveryLimits::default())
            .expect("default Vulkan discovery limits are valid")
    }
}

impl VulkanCommandBufferRegistry {
    pub fn with_limits(limits: VulkanDiscoveryLimits) -> Result<Self, String> {
        validate_limits(limits)?;
        Ok(Self {
            limits,
            buffers: BTreeMap::new(),
            last_allocation_generation: BTreeMap::new(),
            live_pools: BTreeSet::new(),
        })
    }

    pub fn create_pool(&mut self, pool_handle: u64) -> Result<(), String> {
        if pool_handle == 0 || self.live_pools.contains(&pool_handle) {
            return Err("Vulkan command-pool create reused a live or null handle".into());
        }
        if self.live_pools.len() >= self.limits.max_live_command_pools as usize {
            return Err("Vulkan command-pool registry bound exceeded".into());
        }
        self.live_pools.insert(pool_handle);
        Ok(())
    }

    pub fn allocate(
        &mut self,
        handle: u64,
        pool_handle: u64,
        level: VulkanCommandBufferLevel,
    ) -> Result<VulkanCommandBufferIdentity, String> {
        if handle == 0
            || !self.live_pools.contains(&pool_handle)
            || self.buffers.contains_key(&handle)
        {
            return Err("Vulkan command-buffer allocation reused a live or null handle".into());
        }
        if self.buffers.len() >= self.limits.max_live_command_buffers as usize
            || (!self.last_allocation_generation.contains_key(&handle)
                && self.last_allocation_generation.len()
                    >= self.limits.max_tracked_command_buffer_handles as usize)
        {
            return Err("Vulkan command-buffer registry bound exceeded".into());
        }
        let allocation_generation = next_generation(&mut self.last_allocation_generation, handle)?;
        let record = CommandBufferRecord {
            allocation_generation,
            recording_generation: 0,
            pool_handle,
            level,
            state: CommandBufferState::Initial,
            secondaries: Vec::new(),
        };
        self.buffers.insert(handle, record);
        Ok(VulkanCommandBufferIdentity {
            handle,
            allocation_generation,
            recording_generation: 0,
        })
    }

    pub fn begin(&mut self, handle: u64) -> Result<VulkanCommandBufferIdentity, String> {
        let record = self
            .buffers
            .get_mut(&handle)
            .ok_or("Vulkan BeginCommandBuffer referenced an unknown/freed handle")?;
        if record.state == CommandBufferState::Recording {
            return Err("Vulkan BeginCommandBuffer cannot replace an active recording".into());
        }
        // Vulkan permits BeginCommandBuffer on an executable buffer and defines that as an
        // implicit reset. Advancing the recording generation here is what invalidates references
        // retained by primaries from the preceding recording.
        record.recording_generation = record
            .recording_generation
            .checked_add(1)
            .ok_or("Vulkan command-buffer recording generation overflow")?;
        record.secondaries.clear();
        record.state = CommandBufferState::Recording;
        Ok(identity(handle, record))
    }

    pub fn end(&mut self, handle: u64) -> Result<VulkanCommandBufferIdentity, String> {
        let record = self
            .buffers
            .get_mut(&handle)
            .ok_or("Vulkan EndCommandBuffer referenced an unknown/freed handle")?;
        if record.state != CommandBufferState::Recording {
            return Err("Vulkan EndCommandBuffer did not close a recording generation".into());
        }
        record.state = CommandBufferState::Executable;
        Ok(identity(handle, record))
    }

    pub fn execute_secondary(&mut self, primary: u64, secondary: u64) -> Result<(), String> {
        let secondary_record = self
            .buffers
            .get(&secondary)
            .ok_or("Vulkan secondary command buffer is unknown/freed")?;
        if secondary_record.level != VulkanCommandBufferLevel::Secondary
            || secondary_record.state != CommandBufferState::Executable
        {
            return Err("Vulkan secondary command buffer is not executable".into());
        }
        let secondary_identity = identity(secondary, secondary_record);
        let primary_record = self
            .buffers
            .get_mut(&primary)
            .ok_or("Vulkan primary command buffer is unknown/freed")?;
        if primary_record.level != VulkanCommandBufferLevel::Primary
            || primary_record.state != CommandBufferState::Recording
        {
            return Err("vkCmdExecuteCommands target is not a recording primary".into());
        }
        if primary_record.secondaries.len() >= self.limits.max_secondaries_per_primary as usize {
            return Err("Vulkan secondary-command-buffer replay bound exceeded".into());
        }
        primary_record.secondaries.push(secondary_identity);
        Ok(())
    }

    pub fn reset(&mut self, handle: u64) -> Result<(), String> {
        let record = self
            .buffers
            .get_mut(&handle)
            .ok_or("Vulkan command-buffer reset referenced an unknown/freed handle")?;
        if record.state == CommandBufferState::Recording {
            return Err("Vulkan command buffer cannot reset while recording".into());
        }
        record.state = CommandBufferState::Initial;
        record.secondaries.clear();
        Ok(())
    }

    /// Model `vkResetCommandPool`: every allocated command buffer from that pool returns to the
    /// initial state. The backend must turn an untracked/partially tracked pool membership into a
    /// `command_buffer_lifecycle_gaps` counter rather than call this with a guessed subset.
    pub fn reset_pool(&mut self, pool_handle: u64) -> Result<(), String> {
        if !self.live_pools.contains(&pool_handle) {
            return Err("Vulkan command-pool reset referenced an unknown/dead generation".into());
        }
        let matching = self
            .buffers
            .iter()
            .filter_map(|(handle, record)| (record.pool_handle == pool_handle).then_some(*handle))
            .collect::<Vec<_>>();
        if matching.iter().any(|handle| {
            self.buffers
                .get(handle)
                .is_some_and(|record| record.state == CommandBufferState::Recording)
        }) {
            return Err("Vulkan command pool cannot reset while a member is recording".into());
        }
        for handle in matching {
            let record = self
                .buffers
                .get_mut(&handle)
                .expect("matching handles came from this registry");
            record.state = CommandBufferState::Initial;
            record.secondaries.clear();
        }
        Ok(())
    }

    /// Model `vkDestroyCommandPool`, which implicitly frees every command buffer allocated from
    /// that pool. Any primary that retained one of those exact secondary generations will fail the
    /// next submit validation rather than resolve through a reused raw handle.
    pub fn destroy_pool(&mut self, pool_handle: u64) -> Result<(), String> {
        if !self.live_pools.contains(&pool_handle) {
            return Err("Vulkan command-pool destroy referenced an unknown/dead generation".into());
        }
        let matching = self
            .buffers
            .iter()
            .filter_map(|(handle, record)| (record.pool_handle == pool_handle).then_some(*handle))
            .collect::<Vec<_>>();
        if matching.iter().any(|handle| {
            self.buffers
                .get(handle)
                .is_some_and(|record| record.state == CommandBufferState::Recording)
        }) {
            return Err(
                "Vulkan command pool cannot be destroyed while a member is recording".into(),
            );
        }
        for handle in matching {
            self.buffers.remove(&handle);
        }
        self.live_pools.remove(&pool_handle);
        Ok(())
    }

    pub fn free(&mut self, handle: u64) -> Result<(), String> {
        if self
            .buffers
            .get(&handle)
            .is_some_and(|record| record.state == CommandBufferState::Recording)
        {
            return Err("Vulkan command buffer cannot be freed while recording".into());
        }
        self.buffers.remove(&handle).map(|_| ()).ok_or_else(|| {
            "Vulkan command-buffer free referenced an unknown/dead generation".into()
        })
    }

    pub fn validate_submit(&self, primary: VulkanCommandBufferIdentity) -> Result<(), String> {
        let primary_record = self.resolve_exact(primary)?;
        if primary_record.level != VulkanCommandBufferLevel::Primary
            || primary_record.state != CommandBufferState::Executable
        {
            return Err("Vulkan queue submission did not reference an executable primary".into());
        }
        for secondary in &primary_record.secondaries {
            let record = self.resolve_exact(*secondary).map_err(|_| {
                "Vulkan secondary command buffer was reset/freed/reallocated before submit"
                    .to_string()
            })?;
            if record.level != VulkanCommandBufferLevel::Secondary
                || record.state != CommandBufferState::Executable
            {
                return Err("Vulkan secondary command buffer is not executable at submit".into());
            }
        }
        Ok(())
    }

    fn resolve_exact(
        &self,
        expected: VulkanCommandBufferIdentity,
    ) -> Result<&CommandBufferRecord, String> {
        let record = self
            .buffers
            .get(&expected.handle)
            .ok_or("Vulkan command-buffer generation is no longer live")?;
        if record.allocation_generation != expected.allocation_generation
            || record.recording_generation != expected.recording_generation
        {
            return Err("Vulkan command-buffer generation drifted or was reused".into());
        }
        Ok(record)
    }
}

fn identity(handle: u64, record: &CommandBufferRecord) -> VulkanCommandBufferIdentity {
    VulkanCommandBufferIdentity {
        handle,
        allocation_generation: record.allocation_generation,
        recording_generation: record.recording_generation,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VulkanTimelineSignal {
    pub semaphore_handle: u64,
    pub semaphore_generation: u64,
    pub value: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct VulkanAttachmentObservation {
    pub candidate_id: String,
    pub identity: VulkanAttachmentIdentity,
    pub descriptor: VulkanAttachmentDescriptor,
    pub bind_count: u32,
    pub first_command_order: u64,
    pub last_command_order: u64,
}

impl VulkanAttachmentObservation {
    fn validate(&self) -> Result<(), String> {
        validate_token("candidate_id", &self.candidate_id)?;
        if self.identity.image_handle == 0
            || self.identity.image_generation == 0
            || self.identity.view_handle == 0
            || self.identity.view_generation == 0
            || self.bind_count == 0
            || self.first_command_order == 0
            || self.last_command_order < self.first_command_order
        {
            return Err("Vulkan attachment observation identity/order is invalid".into());
        }
        self.descriptor.validate()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct VulkanPresentObservation {
    pub present_seq: u64,
    pub present_queue_id: u64,
    pub swapchain_handle: u64,
    pub swapchain_generation: u64,
    pub image_index: u32,
    /// Monotonic layer-local serials of submissions proven to feed this present.
    pub submission_serials: Vec<u64>,
    /// Timeline signals on the proven dependency path. Values may only increase per generation.
    pub timeline_signals: Vec<VulkanTimelineSignal>,
    pub candidates: Vec<VulkanAttachmentObservation>,
    pub ambiguous_attachment_groups: Vec<Vec<String>>,
}

impl VulkanPresentObservation {
    fn validate_shape(&self, limits: VulkanDiscoveryLimits) -> Result<(), String> {
        self.validate_shape_with_candidate_cap(limits, limits.max_candidates_per_frame as usize)
    }

    fn validate_fixture_input_shape(&self, limits: VulkanDiscoveryLimits) -> Result<(), String> {
        self.validate_shape_with_candidate_cap(
            limits,
            MAX_VULKAN_DISCOVERY_CANDIDATES_PER_FRAME as usize,
        )
    }

    fn validate_shape_with_candidate_cap(
        &self,
        limits: VulkanDiscoveryLimits,
        candidate_cap: usize,
    ) -> Result<(), String> {
        if self.present_seq == 0
            || self.present_queue_id == 0
            || self.swapchain_handle == 0
            || self.swapchain_generation == 0
        {
            return Err("Vulkan present identity is incomplete".into());
        }
        if self.submission_serials.len() > limits.max_submissions_per_present as usize
            || self.timeline_signals.len() > limits.max_timeline_signals_per_present as usize
            || self.candidates.len() > candidate_cap
            || self.ambiguous_attachment_groups.len()
                > limits.max_ambiguous_groups_per_present as usize
            || self
                .ambiguous_attachment_groups
                .iter()
                .any(|group| group.len() > limits.max_ids_per_ambiguous_group as usize)
        {
            return Err("Vulkan present submit/timeline/ambiguity graph bound exceeded".into());
        }
        for candidate in &self.candidates {
            candidate.validate()?;
        }
        for group in &self.ambiguous_attachment_groups {
            if group.len() < 2 {
                return Err(
                    "Vulkan ambiguous attachment group must contain at least two ids".into(),
                );
            }
            for candidate_id in group {
                validate_token("ambiguous candidate_id", candidate_id)?;
            }
        }
        for signal in &self.timeline_signals {
            if signal.semaphore_handle == 0 || signal.semaphore_generation == 0 || signal.value == 0
            {
                return Err("Vulkan timeline signal identity/value is invalid".into());
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VulkanDiscoveryRuntimeCounters {
    pub queue_dropped_frames: u64,
    pub queue_dropped_candidate_rows: u64,
    pub layer_integrity_failures: u64,
    pub command_buffer_lifecycle_gaps: u64,
    pub unresolved_secondary_command_buffers: u64,
    pub object_lifecycle_gaps: u64,
    pub submit_present_association_gaps: u64,
    pub timeline_or_order_drifts: u64,
    pub descriptor_identity_conflicts: u64,
    pub candidate_limit_events: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VulkanDiscoveryBlocker {
    RuntimeBackendUnavailable,
    MissingEntrypointCoverage,
    NoFramesObserved,
    NoCandidatesObserved,
    FrameLimitReached,
    PerFrameCandidateLimitReached,
    CandidateLimitReached,
    QueueSaturated,
    LayerIntegrityFailure,
    CommandBufferLifecycleGap,
    UnresolvedSecondaryCommandBuffer,
    ObjectLifecycleGap,
    PresentAssociationMissing,
    TimelineOrOrderDrift,
    DescriptorIdentityConflict,
    AmbiguousAttachmentIdentity,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct VulkanAttachmentDiscoveryReport {
    pub schema: String,
    pub backend_state: String,
    pub config: VulkanAttachmentDiscoveryConfig,
    /// Static wrapper capabilities implemented by the future layer, not functions observed during
    /// one run. Core/KHR aliases remain required even when a particular application requests only
    /// one spelling. Unit tests may populate this set, but doing so does not turn contract fixtures
    /// into runtime evidence.
    pub layer_entrypoint_capabilities: BTreeSet<String>,
    pub fixture_observations: Vec<VulkanPresentObservation>,
    pub dropped_fixture_frames: u64,
    pub dropped_fixture_candidate_rows: u64,
    pub runtime_counters: VulkanDiscoveryRuntimeCounters,
    pub blockers: BTreeSet<VulkanDiscoveryBlocker>,
    pub runtime_evidence: bool,
    pub observation_complete: bool,
    pub discovery_only: bool,
    pub recording_ready: bool,
    pub deliverable: bool,
    pub profile_generated: bool,
    pub binding_generated: bool,
}

impl VulkanAttachmentDiscoveryReport {
    pub fn validate(&self) -> Result<(), String> {
        self.config.validate()?;
        if self.schema != VULKAN_ATTACHMENT_DISCOVERY_REPORT_SCHEMA_V1
            || self.backend_state != VULKAN_DISCOVERY_BACKEND_STATE
            || self.config.backend_state != self.backend_state
        {
            return Err("unsupported Vulkan discovery-contract report identity".into());
        }
        if self.runtime_evidence
            || self.observation_complete
            || !self.discovery_only
            || self.recording_ready
            || self.deliverable
            || self.profile_generated
            || self.binding_generated
        {
            return Err("Vulkan discovery contract cannot claim runtime evidence, completion, authoring or readiness".into());
        }
        validate_entrypoint_capabilities(&self.layer_entrypoint_capabilities)?;
        if self.fixture_observations.len() > self.config.limits.max_frames as usize {
            return Err(
                "Vulkan discovery report contains more frames than its admitted bound".into(),
            );
        }
        for frame in &self.fixture_observations {
            frame.validate_shape(self.config.limits)?;
        }
        let expected = expected_blockers(self);
        if self.blockers != expected {
            return Err(
                "Vulkan discovery blockers do not match durable loss/integrity facts".into(),
            );
        }
        Ok(())
    }
}

#[derive(Debug)]
pub struct VulkanAttachmentDiscoveryBuilder {
    config: VulkanAttachmentDiscoveryConfig,
    layer_entrypoint_capabilities: BTreeSet<String>,
    frames: Vec<VulkanPresentObservation>,
    dropped_frames: u64,
    dropped_candidate_rows: u64,
    seen_candidates: BTreeMap<String, (VulkanAttachmentIdentity, VulkanAttachmentDescriptor)>,
    builder_counters: VulkanDiscoveryRuntimeCounters,
    last_present_seq: Option<u64>,
    used_submission_serials: BTreeSet<u64>,
    timeline_values: BTreeMap<(u64, u64), u64>,
}

impl VulkanAttachmentDiscoveryBuilder {
    pub fn new(
        config: VulkanAttachmentDiscoveryConfig,
        layer_entrypoint_capabilities: BTreeSet<String>,
    ) -> Result<Self, String> {
        config.validate()?;
        validate_entrypoint_capabilities(&layer_entrypoint_capabilities)?;
        Ok(Self {
            config,
            layer_entrypoint_capabilities,
            frames: Vec::new(),
            dropped_frames: 0,
            dropped_candidate_rows: 0,
            seen_candidates: BTreeMap::new(),
            builder_counters: VulkanDiscoveryRuntimeCounters::default(),
            last_present_seq: None,
            used_submission_serials: BTreeSet::new(),
            timeline_values: BTreeMap::new(),
        })
    }

    /// Observe a synthetic contract fixture. This API is intentionally named `fixture`; schema v1
    /// has no way to ingest or claim actual Vulkan runtime evidence.
    pub fn observe_fixture(&mut self, mut frame: VulkanPresentObservation) -> Result<(), String> {
        frame.validate_fixture_input_shape(self.config.limits)?;
        if self.frames.len() >= self.config.limits.max_frames as usize {
            self.dropped_frames = self.dropped_frames.saturating_add(1);
            self.dropped_candidate_rows = self
                .dropped_candidate_rows
                .saturating_add(frame.candidates.len() as u64);
            return Ok(());
        }
        if frame.candidates.len() > self.config.limits.max_candidates_per_frame as usize {
            let dropped = frame
                .candidates
                .split_off(self.config.limits.max_candidates_per_frame as usize);
            self.dropped_candidate_rows = self
                .dropped_candidate_rows
                .saturating_add(dropped.len() as u64);
        }

        if self
            .last_present_seq
            .is_some_and(|previous| frame.present_seq <= previous)
        {
            self.builder_counters.timeline_or_order_drifts = self
                .builder_counters
                .timeline_or_order_drifts
                .saturating_add(1);
        }
        self.last_present_seq = Some(frame.present_seq);

        if frame.submission_serials.is_empty() {
            self.builder_counters.submit_present_association_gaps = self
                .builder_counters
                .submit_present_association_gaps
                .saturating_add(1);
        }
        let mut previous_serial = None;
        for serial in &frame.submission_serials {
            if *serial == 0
                || previous_serial.is_some_and(|previous| *serial <= previous)
                || !self.used_submission_serials.insert(*serial)
            {
                self.builder_counters.timeline_or_order_drifts = self
                    .builder_counters
                    .timeline_or_order_drifts
                    .saturating_add(1);
            }
            previous_serial = Some(*serial);
        }
        for signal in &frame.timeline_signals {
            let key = (signal.semaphore_handle, signal.semaphore_generation);
            if self
                .timeline_values
                .get(&key)
                .is_some_and(|previous| signal.value <= *previous)
            {
                self.builder_counters.timeline_or_order_drifts = self
                    .builder_counters
                    .timeline_or_order_drifts
                    .saturating_add(1);
            }
            self.timeline_values.insert(key, signal.value);
        }

        for candidate in &frame.candidates {
            match self.seen_candidates.get(&candidate.candidate_id) {
                Some((identity, descriptor))
                    if *identity != candidate.identity || *descriptor != candidate.descriptor =>
                {
                    self.builder_counters.descriptor_identity_conflicts = self
                        .builder_counters
                        .descriptor_identity_conflicts
                        .saturating_add(1);
                }
                None if self.seen_candidates.len()
                    >= self.config.limits.max_candidates as usize =>
                {
                    self.builder_counters.candidate_limit_events = self
                        .builder_counters
                        .candidate_limit_events
                        .saturating_add(1);
                }
                None => {
                    self.seen_candidates.insert(
                        candidate.candidate_id.clone(),
                        (candidate.identity, candidate.descriptor.clone()),
                    );
                }
                Some(_) => {}
            }
        }
        self.frames.push(frame);
        Ok(())
    }

    pub fn finish(
        mut self,
        runtime_counters: VulkanDiscoveryRuntimeCounters,
    ) -> VulkanAttachmentDiscoveryReport {
        self.builder_counters.queue_dropped_frames = self
            .builder_counters
            .queue_dropped_frames
            .saturating_add(runtime_counters.queue_dropped_frames);
        self.builder_counters.queue_dropped_candidate_rows = self
            .builder_counters
            .queue_dropped_candidate_rows
            .saturating_add(runtime_counters.queue_dropped_candidate_rows);
        self.builder_counters.layer_integrity_failures = self
            .builder_counters
            .layer_integrity_failures
            .saturating_add(runtime_counters.layer_integrity_failures);
        self.builder_counters.command_buffer_lifecycle_gaps = self
            .builder_counters
            .command_buffer_lifecycle_gaps
            .saturating_add(runtime_counters.command_buffer_lifecycle_gaps);
        self.builder_counters.unresolved_secondary_command_buffers = self
            .builder_counters
            .unresolved_secondary_command_buffers
            .saturating_add(runtime_counters.unresolved_secondary_command_buffers);
        self.builder_counters.object_lifecycle_gaps = self
            .builder_counters
            .object_lifecycle_gaps
            .saturating_add(runtime_counters.object_lifecycle_gaps);
        self.builder_counters.submit_present_association_gaps = self
            .builder_counters
            .submit_present_association_gaps
            .saturating_add(runtime_counters.submit_present_association_gaps);
        self.builder_counters.timeline_or_order_drifts = self
            .builder_counters
            .timeline_or_order_drifts
            .saturating_add(runtime_counters.timeline_or_order_drifts);
        self.builder_counters.descriptor_identity_conflicts = self
            .builder_counters
            .descriptor_identity_conflicts
            .saturating_add(runtime_counters.descriptor_identity_conflicts);
        self.builder_counters.candidate_limit_events = self
            .builder_counters
            .candidate_limit_events
            .saturating_add(runtime_counters.candidate_limit_events);

        let mut report = VulkanAttachmentDiscoveryReport {
            schema: VULKAN_ATTACHMENT_DISCOVERY_REPORT_SCHEMA_V1.into(),
            backend_state: VULKAN_DISCOVERY_BACKEND_STATE.into(),
            config: self.config,
            layer_entrypoint_capabilities: self.layer_entrypoint_capabilities,
            fixture_observations: self.frames,
            dropped_fixture_frames: self.dropped_frames,
            dropped_fixture_candidate_rows: self.dropped_candidate_rows,
            runtime_counters: self.builder_counters,
            blockers: BTreeSet::new(),
            runtime_evidence: false,
            observation_complete: false,
            discovery_only: true,
            recording_ready: false,
            deliverable: false,
            profile_generated: false,
            binding_generated: false,
        };
        report.blockers = expected_blockers(&report);
        report
    }
}

fn validate_entrypoint_capabilities(entrypoints: &BTreeSet<String>) -> Result<(), String> {
    if entrypoints.len() > MAX_VULKAN_LAYER_ENTRYPOINT_CAPABILITIES
        || entrypoints.iter().any(|entry| {
            entry.is_empty()
                || entry.len() > 128
                || !entry
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        })
    {
        return Err("Vulkan layer entrypoint capability set is invalid or unbounded".into());
    }
    Ok(())
}

fn expected_blockers(report: &VulkanAttachmentDiscoveryReport) -> BTreeSet<VulkanDiscoveryBlocker> {
    let mut blockers = BTreeSet::from([VulkanDiscoveryBlocker::RuntimeBackendUnavailable]);
    if REQUIRED_VULKAN_DISCOVERY_ENTRYPOINTS
        .iter()
        .any(|entry| !report.layer_entrypoint_capabilities.contains(*entry))
    {
        blockers.insert(VulkanDiscoveryBlocker::MissingEntrypointCoverage);
    }
    if report.fixture_observations.is_empty() {
        blockers.insert(VulkanDiscoveryBlocker::NoFramesObserved);
    }
    if !report
        .fixture_observations
        .iter()
        .any(|frame| !frame.candidates.is_empty())
    {
        blockers.insert(VulkanDiscoveryBlocker::NoCandidatesObserved);
    }
    if report.dropped_fixture_frames > 0 {
        blockers.insert(VulkanDiscoveryBlocker::FrameLimitReached);
    }
    if report.dropped_fixture_candidate_rows > 0 {
        blockers.insert(VulkanDiscoveryBlocker::PerFrameCandidateLimitReached);
    }

    let mut candidate_identities =
        BTreeMap::<&str, (&VulkanAttachmentIdentity, &VulkanAttachmentDescriptor)>::new();
    let mut last_present_seq = None;
    let mut submission_serials = BTreeSet::new();
    let mut timeline_values = BTreeMap::<(u64, u64), u64>::new();
    for frame in &report.fixture_observations {
        if last_present_seq.is_some_and(|previous| frame.present_seq <= previous) {
            blockers.insert(VulkanDiscoveryBlocker::TimelineOrOrderDrift);
        }
        last_present_seq = Some(frame.present_seq);
        if frame.submission_serials.is_empty() {
            blockers.insert(VulkanDiscoveryBlocker::PresentAssociationMissing);
        }
        let mut previous_serial = None;
        for serial in &frame.submission_serials {
            if *serial == 0
                || previous_serial.is_some_and(|previous| *serial <= previous)
                || !submission_serials.insert(*serial)
            {
                blockers.insert(VulkanDiscoveryBlocker::TimelineOrOrderDrift);
            }
            previous_serial = Some(*serial);
        }
        for signal in &frame.timeline_signals {
            let key = (signal.semaphore_handle, signal.semaphore_generation);
            if timeline_values
                .get(&key)
                .is_some_and(|previous| signal.value <= *previous)
            {
                blockers.insert(VulkanDiscoveryBlocker::TimelineOrOrderDrift);
            }
            timeline_values.insert(key, signal.value);
        }
        for (index, candidate) in frame.candidates.iter().enumerate() {
            if let Some((identity, descriptor)) =
                candidate_identities.get(candidate.candidate_id.as_str())
                && (**identity != candidate.identity || **descriptor != candidate.descriptor)
            {
                blockers.insert(VulkanDiscoveryBlocker::DescriptorIdentityConflict);
            }
            candidate_identities.insert(
                candidate.candidate_id.as_str(),
                (&candidate.identity, &candidate.descriptor),
            );
            if frame.candidates[index + 1..].iter().any(|other| {
                other.candidate_id != candidate.candidate_id
                    && other.identity != candidate.identity
                    && other.descriptor == candidate.descriptor
            }) {
                blockers.insert(VulkanDiscoveryBlocker::AmbiguousAttachmentIdentity);
            }
        }
    }
    if candidate_identities.len() > report.config.limits.max_candidates as usize {
        blockers.insert(VulkanDiscoveryBlocker::CandidateLimitReached);
    }
    let counters = report.runtime_counters;
    if counters.queue_dropped_frames > 0 || counters.queue_dropped_candidate_rows > 0 {
        blockers.insert(VulkanDiscoveryBlocker::QueueSaturated);
    }
    if counters.layer_integrity_failures > 0 {
        blockers.insert(VulkanDiscoveryBlocker::LayerIntegrityFailure);
    }
    if counters.command_buffer_lifecycle_gaps > 0 {
        blockers.insert(VulkanDiscoveryBlocker::CommandBufferLifecycleGap);
    }
    if counters.unresolved_secondary_command_buffers > 0 {
        blockers.insert(VulkanDiscoveryBlocker::UnresolvedSecondaryCommandBuffer);
    }
    if counters.object_lifecycle_gaps > 0 {
        blockers.insert(VulkanDiscoveryBlocker::ObjectLifecycleGap);
    }
    if counters.submit_present_association_gaps > 0 {
        blockers.insert(VulkanDiscoveryBlocker::PresentAssociationMissing);
    }
    if counters.timeline_or_order_drifts > 0 {
        blockers.insert(VulkanDiscoveryBlocker::TimelineOrOrderDrift);
    }
    if counters.descriptor_identity_conflicts > 0 {
        blockers.insert(VulkanDiscoveryBlocker::DescriptorIdentityConflict);
    }
    if counters.candidate_limit_events > 0 {
        blockers.insert(VulkanDiscoveryBlocker::CandidateLimitReached);
    }
    if report
        .fixture_observations
        .iter()
        .any(|frame| !frame.ambiguous_attachment_groups.is_empty())
    {
        blockers.insert(VulkanDiscoveryBlocker::AmbiguousAttachmentIdentity);
    }
    blockers
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest(byte: char) -> String {
        format!("sha256:{}", byte.to_string().repeat(64))
    }

    fn config() -> VulkanAttachmentDiscoveryConfig {
        VulkanAttachmentDiscoveryConfig {
            schema: VULKAN_ATTACHMENT_DISCOVERY_CONFIG_SCHEMA_V1.into(),
            backend_state: VULKAN_DISCOVERY_BACKEND_STATE.into(),
            discovery_id: "contract-1".into(),
            game_id: "generic-vulkan-game".into(),
            app_id: "123".into(),
            build_id: "456".into(),
            executable_basename: "Game.exe".into(),
            executable_sha256: digest('a'),
            process_launch_id: "launch-1".into(),
            process_start_filetime: 1,
            vulkan_loader_sha256: digest('b'),
            layer_manifest_sha256: digest('c'),
            layer_library_sha256: digest('d'),
            runtime_gate_report_sha256: digest('e'),
            rights_evidence_sha256: digest('f'),
            limits: VulkanDiscoveryLimits {
                max_frames: 2,
                max_candidates: 2,
                max_candidates_per_frame: 2,
                max_ids_per_ambiguous_group: 2,
                ..VulkanDiscoveryLimits::default()
            },
        }
    }

    fn all_entrypoints() -> BTreeSet<String> {
        REQUIRED_VULKAN_DISCOVERY_ENTRYPOINTS
            .iter()
            .map(|entry| (*entry).to_string())
            .collect()
    }

    fn image() -> VulkanImageDescriptor {
        VulkanImageDescriptor {
            format: 126,
            width: 1920,
            height: 1080,
            depth: 1,
            mip_levels: 1,
            array_layers: 1,
            samples: 1,
            usage: 0x20,
        }
    }

    fn view() -> VulkanImageViewDescriptor {
        VulkanImageViewDescriptor {
            format: 126,
            view_type: 1,
            aspect_mask: 0x2,
            base_mip_level: 0,
            level_count: 1,
            base_array_layer: 0,
            layer_count: 1,
        }
    }

    fn candidate(id: &str, identity: VulkanAttachmentIdentity) -> VulkanAttachmentObservation {
        VulkanAttachmentObservation {
            candidate_id: id.into(),
            identity,
            descriptor: VulkanAttachmentDescriptor {
                image: image(),
                view: view(),
                rendering_path: VulkanRenderingPath::DynamicRendering,
            },
            bind_count: 1,
            first_command_order: 1,
            last_command_order: 1,
        }
    }

    fn frame(
        present_seq: u64,
        submission_serial: u64,
        candidate: VulkanAttachmentObservation,
    ) -> VulkanPresentObservation {
        VulkanPresentObservation {
            present_seq,
            present_queue_id: 1,
            swapchain_handle: 2,
            swapchain_generation: 1,
            image_index: 0,
            submission_serials: vec![submission_serial],
            timeline_signals: vec![VulkanTimelineSignal {
                semaphore_handle: 3,
                semaphore_generation: 1,
                value: present_seq,
            }],
            candidates: vec![candidate],
            ambiguous_attachment_groups: Vec::new(),
        }
    }

    #[test]
    fn config_binds_exact_build_loader_layer_and_runtime_gate_hashes() {
        config().validate().unwrap();
        for field in 0..6 {
            let mut weakened = config();
            match field {
                0 => weakened.executable_sha256 = "sha256:bad".into(),
                1 => weakened.vulkan_loader_sha256 = "sha256:bad".into(),
                2 => weakened.layer_manifest_sha256 = "sha256:bad".into(),
                3 => weakened.layer_library_sha256 = "sha256:bad".into(),
                4 => weakened.runtime_gate_report_sha256 = "sha256:bad".into(),
                5 => weakened.rights_evidence_sha256 = "sha256:bad".into(),
                _ => unreachable!(),
            }
            assert!(weakened.validate().is_err());
        }
        let mut wrong_state = config();
        wrong_state.backend_state = "available".into();
        assert!(wrong_state.validate().is_err());
        let mut unbounded = config();
        unbounded.limits.max_live_images = MAX_VULKAN_LIVE_IMAGES + 1;
        assert!(unbounded.validate().is_err());
    }

    #[test]
    fn layer_coverage_requires_submit2_khr_secondary_reset_free_destroy_and_present() {
        let critical = [
            "vkQueueSubmit2KHR",
            "vkCmdExecuteCommands",
            "vkResetCommandBuffer",
            "vkResetCommandPool",
            "vkFreeCommandBuffers",
            "vkDestroyImage",
            "vkDestroyImageView",
            "vkQueuePresentKHR",
        ];
        for missing in critical {
            let mut entrypoints = all_entrypoints();
            entrypoints.remove(missing);
            let report = VulkanAttachmentDiscoveryBuilder::new(config(), entrypoints)
                .unwrap()
                .finish(VulkanDiscoveryRuntimeCounters::default());
            assert!(
                report
                    .blockers
                    .contains(&VulkanDiscoveryBlocker::MissingEntrypointCoverage),
                "missing {missing} was not blocked"
            );
            report.validate().unwrap();
        }
    }

    #[test]
    fn capability_and_present_dependency_graph_inputs_are_bounded() {
        let mut oversized_capabilities = all_entrypoints();
        for index in 0..=MAX_VULKAN_LAYER_ENTRYPOINT_CAPABILITIES {
            oversized_capabilities.insert(format!("vkSynthetic{index}"));
        }
        assert!(VulkanAttachmentDiscoveryBuilder::new(config(), oversized_capabilities).is_err());

        let identity = VulkanAttachmentIdentity {
            image_handle: 10,
            image_generation: 1,
            view_handle: 20,
            view_generation: 1,
        };
        let mut bounded = config();
        bounded.limits.max_submissions_per_present = 1;
        bounded.limits.max_timeline_signals_per_present = 1;
        bounded.limits.max_ambiguous_groups_per_present = 1;
        bounded.limits.max_ids_per_ambiguous_group = 2;
        let mut builder =
            VulkanAttachmentDiscoveryBuilder::new(bounded, all_entrypoints()).unwrap();
        let mut overfull = frame(1, 1, candidate("depth-1", identity));
        overfull.submission_serials.push(2);
        assert!(builder.observe_fixture(overfull).is_err());

        let mut overfull = frame(1, 1, candidate("depth-1", identity));
        overfull.timeline_signals.push(VulkanTimelineSignal {
            semaphore_handle: 4,
            semaphore_generation: 1,
            value: 1,
        });
        assert!(builder.observe_fixture(overfull).is_err());
    }

    #[test]
    fn image_and_view_registry_rejects_dead_or_reused_generations() {
        let mut registry = VulkanAttachmentRegistry::default();
        assert_eq!(registry.create_image(10, image()).unwrap(), 1);
        let first = registry.create_view(20, 10, view()).unwrap();
        registry
            .resolve(first, VulkanRenderingPath::RenderPass)
            .unwrap();
        assert!(registry.destroy_image(10).is_err());
        registry.destroy_view(20).unwrap();
        assert!(
            registry
                .resolve(first, VulkanRenderingPath::RenderPass)
                .is_err()
        );
        registry.destroy_image(10).unwrap();
        assert_eq!(registry.create_image(10, image()).unwrap(), 2);
        let second = registry.create_view(20, 10, view()).unwrap();
        assert_eq!(second.image_generation, 2);
        assert_eq!(second.view_generation, 2);
        assert!(
            registry
                .resolve(first, VulkanRenderingPath::RenderPass)
                .is_err()
        );
    }

    #[test]
    fn image_view_and_generation_maps_are_bounded() {
        let limits = VulkanDiscoveryLimits {
            max_live_images: 1,
            max_tracked_image_handles: 2,
            max_live_image_views: 1,
            max_tracked_image_view_handles: 2,
            ..VulkanDiscoveryLimits::default()
        };
        let mut registry = VulkanAttachmentRegistry::with_limits(limits).unwrap();
        registry.create_image(10, image()).unwrap();
        assert!(registry.create_image(11, image()).is_err());
        let first = registry.create_view(20, 10, view()).unwrap();
        assert!(registry.create_view(21, 10, view()).is_err());
        registry.destroy_view(first.view_handle).unwrap();
        registry.destroy_image(first.image_handle).unwrap();
        registry.create_image(11, image()).unwrap();
        let second = registry.create_view(21, 11, view()).unwrap();
        registry.destroy_view(second.view_handle).unwrap();
        registry.destroy_image(second.image_handle).unwrap();
        assert!(registry.create_image(12, image()).is_err());

        registry.create_image(10, image()).unwrap();
        registry.create_view(20, 10, view()).unwrap();
        registry.destroy_view(20).unwrap();
        assert!(registry.create_view(22, 10, view()).is_err());
    }

    #[test]
    fn secondary_reset_free_and_reallocation_invalidate_recorded_primary_reference() {
        let mut buffers = VulkanCommandBufferRegistry::default();
        buffers.create_pool(10).unwrap();
        buffers.create_pool(20).unwrap();
        buffers
            .allocate(100, 10, VulkanCommandBufferLevel::Secondary)
            .unwrap();
        buffers.begin(100).unwrap();
        buffers.end(100).unwrap();
        buffers
            .allocate(200, 20, VulkanCommandBufferLevel::Primary)
            .unwrap();
        buffers.begin(200).unwrap();
        buffers.execute_secondary(200, 100).unwrap();
        let primary = buffers.end(200).unwrap();
        buffers.validate_submit(primary).unwrap();

        buffers.reset(100).unwrap();
        assert!(buffers.validate_submit(primary).is_err());
        buffers.begin(100).unwrap();
        buffers.end(100).unwrap();
        assert!(buffers.validate_submit(primary).is_err());
        buffers.free(100).unwrap();
        assert!(buffers.validate_submit(primary).is_err());
        buffers.destroy_pool(10).unwrap();
        buffers.create_pool(10).unwrap();
        let reallocated = buffers
            .allocate(100, 10, VulkanCommandBufferLevel::Secondary)
            .unwrap();
        assert_eq!(reallocated.allocation_generation, 2);
        assert!(buffers.validate_submit(primary).is_err());
    }

    #[test]
    fn command_pool_reset_and_implicit_begin_advance_or_invalidate_recordings() {
        let mut buffers = VulkanCommandBufferRegistry::default();
        buffers.create_pool(10).unwrap();
        buffers
            .allocate(100, 10, VulkanCommandBufferLevel::Secondary)
            .unwrap();
        buffers.begin(100).unwrap();
        let first = buffers.end(100).unwrap();

        // Begin on an executable command buffer is Vulkan's implicit reset path.
        buffers.begin(100).unwrap();
        let second = buffers.end(100).unwrap();
        assert_ne!(first.recording_generation, second.recording_generation);
        assert!(buffers.resolve_exact(first).is_err());

        buffers.reset_pool(10).unwrap();
        assert!(buffers.resolve_exact(second).is_ok());
        assert!(buffers.validate_submit(second).is_err());
        assert!(buffers.reset_pool(999).is_err());
    }

    #[test]
    fn command_pool_destroy_implicitly_frees_secondary_generation() {
        let mut buffers = VulkanCommandBufferRegistry::default();
        buffers.create_pool(10).unwrap();
        buffers.create_pool(20).unwrap();
        buffers
            .allocate(100, 10, VulkanCommandBufferLevel::Secondary)
            .unwrap();
        buffers.begin(100).unwrap();
        buffers.end(100).unwrap();
        buffers
            .allocate(200, 20, VulkanCommandBufferLevel::Primary)
            .unwrap();
        buffers.begin(200).unwrap();
        buffers.execute_secondary(200, 100).unwrap();
        let primary = buffers.end(200).unwrap();
        buffers.validate_submit(primary).unwrap();

        buffers.destroy_pool(10).unwrap();
        assert!(buffers.validate_submit(primary).is_err());
        buffers.create_pool(30).unwrap();
        let reallocated = buffers
            .allocate(100, 30, VulkanCommandBufferLevel::Secondary)
            .unwrap();
        assert_eq!(reallocated.allocation_generation, 2);
        assert!(buffers.validate_submit(primary).is_err());
    }

    #[test]
    fn command_pool_buffer_generation_and_secondary_lists_are_bounded() {
        let limits = VulkanDiscoveryLimits {
            max_live_command_pools: 1,
            max_live_command_buffers: 3,
            max_tracked_command_buffer_handles: 3,
            max_secondaries_per_primary: 1,
            ..VulkanDiscoveryLimits::default()
        };
        let mut buffers = VulkanCommandBufferRegistry::with_limits(limits).unwrap();
        buffers.create_pool(10).unwrap();
        assert!(buffers.create_pool(11).is_err());
        buffers
            .allocate(100, 10, VulkanCommandBufferLevel::Secondary)
            .unwrap();
        buffers.begin(100).unwrap();
        buffers.end(100).unwrap();
        buffers
            .allocate(101, 10, VulkanCommandBufferLevel::Secondary)
            .unwrap();
        buffers.begin(101).unwrap();
        buffers.end(101).unwrap();
        buffers
            .allocate(200, 10, VulkanCommandBufferLevel::Primary)
            .unwrap();
        assert!(
            buffers
                .allocate(201, 10, VulkanCommandBufferLevel::Primary)
                .is_err()
        );
        buffers.begin(200).unwrap();
        buffers.execute_secondary(200, 100).unwrap();
        assert!(buffers.execute_secondary(200, 101).is_err());
        buffers.end(200).unwrap();
        buffers.destroy_pool(10).unwrap();
        buffers.create_pool(11).unwrap();
        assert!(
            buffers
                .allocate(300, 11, VulkanCommandBufferLevel::Primary)
                .is_err()
        );
    }

    #[test]
    fn present_association_timeline_and_descriptor_drift_are_durable_blockers() {
        let identity = VulkanAttachmentIdentity {
            image_handle: 10,
            image_generation: 1,
            view_handle: 20,
            view_generation: 1,
        };
        let mut builder =
            VulkanAttachmentDiscoveryBuilder::new(config(), all_entrypoints()).unwrap();
        builder
            .observe_fixture(frame(2, 2, candidate("depth-1", identity)))
            .unwrap();
        let mut regressed = frame(1, 2, candidate("depth-1", identity));
        regressed.timeline_signals[0].value = 1;
        regressed.candidates[0].descriptor.image.width = 1280;
        builder.observe_fixture(regressed).unwrap();
        let report = builder.finish(VulkanDiscoveryRuntimeCounters::default());
        assert!(
            report
                .blockers
                .contains(&VulkanDiscoveryBlocker::TimelineOrOrderDrift)
        );
        assert!(
            report
                .blockers
                .contains(&VulkanDiscoveryBlocker::DescriptorIdentityConflict)
        );
        report.validate().unwrap();
        let mut forged = report;
        forged.runtime_counters.timeline_or_order_drifts = 0;
        forged.runtime_counters.descriptor_identity_conflicts = 0;
        forged
            .blockers
            .remove(&VulkanDiscoveryBlocker::TimelineOrOrderDrift);
        forged
            .blockers
            .remove(&VulkanDiscoveryBlocker::DescriptorIdentityConflict);
        assert!(forged.validate().is_err());

        let mut missing =
            VulkanAttachmentDiscoveryBuilder::new(config(), all_entrypoints()).unwrap();
        let mut no_submit = frame(1, 1, candidate("depth-1", identity));
        no_submit.submission_serials.clear();
        missing.observe_fixture(no_submit).unwrap();
        let report = missing.finish(VulkanDiscoveryRuntimeCounters::default());
        assert!(
            report
                .blockers
                .contains(&VulkanDiscoveryBlocker::PresentAssociationMissing)
        );
        report.validate().unwrap();
        let mut forged = report;
        forged.runtime_counters.submit_present_association_gaps = 0;
        forged
            .blockers
            .remove(&VulkanDiscoveryBlocker::PresentAssociationMissing);
        assert!(forged.validate().is_err());
    }

    #[test]
    fn every_runtime_loss_family_fails_closed_and_cannot_be_erased() {
        let counters = VulkanDiscoveryRuntimeCounters {
            queue_dropped_frames: 1,
            queue_dropped_candidate_rows: 1,
            layer_integrity_failures: 1,
            command_buffer_lifecycle_gaps: 1,
            unresolved_secondary_command_buffers: 1,
            object_lifecycle_gaps: 1,
            submit_present_association_gaps: 1,
            timeline_or_order_drifts: 1,
            descriptor_identity_conflicts: 1,
            candidate_limit_events: 1,
        };
        let report = VulkanAttachmentDiscoveryBuilder::new(config(), all_entrypoints())
            .unwrap()
            .finish(counters);
        for blocker in [
            VulkanDiscoveryBlocker::QueueSaturated,
            VulkanDiscoveryBlocker::LayerIntegrityFailure,
            VulkanDiscoveryBlocker::CommandBufferLifecycleGap,
            VulkanDiscoveryBlocker::UnresolvedSecondaryCommandBuffer,
            VulkanDiscoveryBlocker::ObjectLifecycleGap,
            VulkanDiscoveryBlocker::PresentAssociationMissing,
            VulkanDiscoveryBlocker::TimelineOrOrderDrift,
            VulkanDiscoveryBlocker::DescriptorIdentityConflict,
            VulkanDiscoveryBlocker::CandidateLimitReached,
        ] {
            assert!(report.blockers.contains(&blocker));
        }
        let mut forged = report;
        forged.blockers.clear();
        assert!(forged.validate().is_err());
    }

    #[test]
    fn contract_fixture_can_never_claim_runtime_completion_or_ready() {
        let identity = VulkanAttachmentIdentity {
            image_handle: 10,
            image_generation: 1,
            view_handle: 20,
            view_generation: 1,
        };
        let mut builder =
            VulkanAttachmentDiscoveryBuilder::new(config(), all_entrypoints()).unwrap();
        builder
            .observe_fixture(frame(1, 1, candidate("depth-1", identity)))
            .unwrap();
        let report = builder.finish(VulkanDiscoveryRuntimeCounters::default());
        assert_eq!(
            report.blockers,
            BTreeSet::from([VulkanDiscoveryBlocker::RuntimeBackendUnavailable])
        );
        report.validate().unwrap();
        for field in 0..6 {
            let mut forged = report.clone();
            match field {
                0 => forged.runtime_evidence = true,
                1 => forged.observation_complete = true,
                2 => forged.recording_ready = true,
                3 => forged.deliverable = true,
                4 => forged.profile_generated = true,
                5 => forged.binding_generated = true,
                _ => unreachable!(),
            }
            assert!(forged.validate().is_err());
        }
    }

    #[test]
    fn frame_and_candidate_bounds_remain_visible_in_report() {
        let identity = VulkanAttachmentIdentity {
            image_handle: 10,
            image_generation: 1,
            view_handle: 20,
            view_generation: 1,
        };
        let mut builder =
            VulkanAttachmentDiscoveryBuilder::new(config(), all_entrypoints()).unwrap();
        let mut overfull = frame(1, 1, candidate("depth-1", identity));
        let mut second = candidate(
            "depth-2",
            VulkanAttachmentIdentity {
                view_handle: 21,
                ..identity
            },
        );
        second.first_command_order = 2;
        second.last_command_order = 2;
        let mut third = candidate(
            "depth-3",
            VulkanAttachmentIdentity {
                view_handle: 22,
                ..identity
            },
        );
        third.first_command_order = 3;
        third.last_command_order = 3;
        overfull.candidates.extend([second, third]);
        builder.observe_fixture(overfull).unwrap();
        builder
            .observe_fixture(frame(2, 2, candidate("depth-1", identity)))
            .unwrap();
        builder
            .observe_fixture(frame(3, 3, candidate("depth-1", identity)))
            .unwrap();
        let report = builder.finish(VulkanDiscoveryRuntimeCounters::default());
        assert!(
            report
                .blockers
                .contains(&VulkanDiscoveryBlocker::PerFrameCandidateLimitReached)
        );
        assert!(
            report
                .blockers
                .contains(&VulkanDiscoveryBlocker::FrameLimitReached)
        );
        report.validate().unwrap();
    }

    #[test]
    fn forged_oversized_reports_and_fixture_inputs_are_rejected_before_scanning() {
        let identity = VulkanAttachmentIdentity {
            image_handle: 10,
            image_generation: 1,
            view_handle: 20,
            view_generation: 1,
        };
        let mut builder =
            VulkanAttachmentDiscoveryBuilder::new(config(), all_entrypoints()).unwrap();
        builder
            .observe_fixture(frame(1, 1, candidate("depth-1", identity)))
            .unwrap();
        let report = builder.finish(VulkanDiscoveryRuntimeCounters::default());

        let mut too_many_frames = report.clone();
        too_many_frames
            .fixture_observations
            .push(frame(2, 2, candidate("depth-1", identity)));
        too_many_frames
            .fixture_observations
            .push(frame(3, 3, candidate("depth-1", identity)));
        assert!(too_many_frames.validate().is_err());

        let mut too_many_stored_candidates = report;
        let stored_frame = &mut too_many_stored_candidates.fixture_observations[0];
        stored_frame.candidates.push(candidate("depth-2", identity));
        stored_frame.candidates.push(candidate("depth-3", identity));
        assert!(too_many_stored_candidates.validate().is_err());

        let mut oversized_input = frame(1, 1, candidate("depth-0", identity));
        for index in 1..=MAX_VULKAN_DISCOVERY_CANDIDATES_PER_FRAME {
            oversized_input
                .candidates
                .push(candidate(&format!("depth-{index}"), identity));
        }
        let mut builder =
            VulkanAttachmentDiscoveryBuilder::new(config(), all_entrypoints()).unwrap();
        assert!(builder.observe_fixture(oversized_input).is_err());
    }
}
