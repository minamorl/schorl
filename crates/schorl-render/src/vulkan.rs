//! Vulkan の instance / device / queue を所有する面。
//!
//! 二通りの作り方を持つ。
//!
//! 1. [`VulkanContext::standalone`] — OpenXR を通さず自前で作る。dmabuf の
//!    import が成立するかを測るときに使う。
//! 2. [`VulkanContext::through_openxr`] — `XR_KHR_vulkan_enable2` の逐語どおり、
//!    ランタイムに `VkInstance` と `VkDevice` を作らせる。`xrCreateVulkanInstanceKHR`
//!    / `xrCreateVulkanDeviceKHR` を通すと、ランタイムが要求する Vulkan 拡張が
//!    自動で足される。session を開く前に通しておかなければならない。
//!
//! `host.created_resource_lifecycle` と `house.resource_lifecycle.*` により、
//! 所有した instance / device は `Drop` で必ず返す。`Drop` は失敗を報告できないので、
//! 明示的に失敗を見たい呼び手のために [`VulkanContext::wait_idle`] を別に開けてある。

use ash::vk::{self, Handle as _};
use schorl_core::error::{Error, ErrorCode, Result};
use schorl_core::id::TraceId;

/// この crate が device へ必ず要求する拡張。
///
/// dmabuf の import 経路 (`free schorl.compositor.buffer_import_path` の選択) が
/// これだけの拡張に乗っている。無いホストでは import を作らず、shm の退路だけを
/// 残して封筒で知らせる。
pub const DMABUF_DEVICE_EXTENSIONS: [&core::ffi::CStr; 4] = [
    ash::khr::external_memory_fd::NAME,
    ash::ext::external_memory_dma_buf::NAME,
    ash::ext::image_drm_format_modifier::NAME,
    ash::ext::queue_family_foreign::NAME,
];

/// 実際に組めた Vulkan について機械が言い切れる事実。
///
/// 「Vulkan が使えた」という主張の中身を数字と綴りで持つ。要約ではなく観測を
/// 報告へ貼るために在る。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VulkanDeviceFacts {
    /// 物理デバイスの名前。
    pub device_name: String,
    /// `VkPhysicalDeviceProperties::apiVersion` を major.minor.patch で綴ったもの。
    pub api_version: String,
    /// ドライバの版 (生の u32 を十進で)。
    pub driver_version: u32,
    /// 使った graphics queue family の番号。
    pub queue_family_index: u32,
    /// 物理デバイスが並べた device 拡張の数。
    pub advertised_device_extension_count: usize,
    /// dmabuf の import に要る拡張が全部そろっていたか。
    pub dmabuf_extensions_present: bool,
    /// 実際に有効化した device 拡張の綴り。
    pub enabled_device_extensions: Vec<String>,
}

/// Vulkan の面。
///
/// `Debug` は手書きである。ash の型は生の handle しか持たないので、
/// 覗いても意味の無い数字が出るだけだから、事実の側だけを出す。
pub struct VulkanContext {
    entry: ash::Entry,
    instance: ash::Instance,
    physical_device: vk::PhysicalDevice,
    device: ash::Device,
    queue: vk::Queue,
    queue_family_index: u32,
    memory_properties: vk::PhysicalDeviceMemoryProperties,
    external_memory_fd: Option<ash::khr::external_memory_fd::Device>,
    drm_format_modifier: Option<ash::ext::image_drm_format_modifier::Device>,
    facts: VulkanDeviceFacts,
}

impl core::fmt::Debug for VulkanContext {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("VulkanContext")
            .field("facts", &self.facts)
            .finish_non_exhaustive()
    }
}

impl VulkanContext {
    /// OpenXR を通さずに自前で組む。
    ///
    /// dmabuf の import が成立するかだけを測りたいとき、ランタイムを起こさずに
    /// 済ませるための入口。XR セッションには使えない (ランタイムが要求する
    /// 拡張が入らないため)。そちらは [`VulkanContext::through_openxr`]。
    pub fn standalone(application_name: &str) -> Result<Self> {
        let entry = load_entry()?;
        let app_name = cstring(application_name)?;
        let app_info = vk::ApplicationInfo::default()
            .application_name(&app_name)
            .application_version(1)
            .engine_version(1)
            .api_version(vk::make_api_version(0, 1, 1, 0));
        let create_info = vk::InstanceCreateInfo::default().application_info(&app_info);
        // SAFETY: create_info は上でこの場に組んだもので、参照先はこの呼び出しの
        // 間だけ生きていればよい。allocator は既定 (None)。
        let instance = unsafe { entry.create_instance(&create_info, None) }
            .map_err(|e| vulkan_failure("vkCreateInstance failed", e))?;
        let picked = match pick_physical_device(&instance) {
            Ok(picked) => picked,
            Err(e) => {
                // SAFETY: instance はこの関数が作ったもので、他に生きた子が無い。
                unsafe { instance.destroy_instance(None) };
                return Err(e);
            }
        };
        let (physical_device, queue_family_index) = picked;
        let wanted = present_dmabuf_extensions(&instance, physical_device)?;
        let names: Vec<*const core::ffi::c_char> = wanted.iter().map(|n| n.as_ptr()).collect();
        let priorities = [1.0f32];
        let queue_infos = [vk::DeviceQueueCreateInfo::default()
            .queue_family_index(queue_family_index)
            .queue_priorities(&priorities)];
        let device_info = vk::DeviceCreateInfo::default()
            .queue_create_infos(&queue_infos)
            .enabled_extension_names(&names);
        // SAFETY: physical_device は上の instance から引いたもの。
        let device = match unsafe { instance.create_device(physical_device, &device_info, None) } {
            Ok(device) => device,
            Err(e) => {
                // SAFETY: 同上。
                unsafe { instance.destroy_instance(None) };
                return Err(vulkan_failure("vkCreateDevice failed", e));
            }
        };
        // SAFETY: 上で作った instance / device / physical_device の組であり、
        // queue_family_index は同じ physical_device から選んだもの。
        Ok(unsafe {
            Self::adopt(
                entry,
                instance,
                physical_device,
                device,
                queue_family_index,
                &wanted,
            )
        })
    }

    /// `XR_KHR_vulkan_enable2` の経路で、ランタイムに作らせて組む。
    ///
    /// 逐語の手順は openxr crate の `examples/vulkan.rs` と同じ順序である:
    /// `graphics_requirements` → `create_vulkan_instance` →
    /// `vulkan_graphics_device` → `create_vulkan_device`。
    /// requirements を先に引かなければ instance を作れない。
    ///
    /// # Safety
    ///
    /// `xr_instance` は生きていること。返る [`VulkanContext`] は `xr_instance` より
    /// 先に落とさないこと (OpenXR が触りうる Vulkan 資源を先に壊さないため)。
    pub unsafe fn through_openxr(
        xr_instance: &openxr::Instance,
        system: openxr::SystemId,
    ) -> Result<Self> {
        let entry = load_entry()?;
        let reqs = xr_instance
            .graphics_requirements::<openxr::Vulkan>(system)
            .map_err(|e| openxr_failure("xrGetVulkanGraphicsRequirements2KHR failed", e))?;
        let target = openxr::Version::new(1, 1, 0);
        if target < reqs.min_api_version_supported
            || target.major() > reqs.max_api_version_supported.major()
        {
            return Err(Error::new(
                ErrorCode::Unsupported,
                "the OpenXR runtime does not accept Vulkan 1.1 for this system",
                TraceId::unattributed(),
            )
            .with_detail(
                "min_api_version",
                reqs.min_api_version_supported.to_string(),
            )
            .with_detail(
                "max_api_version",
                reqs.max_api_version_supported.to_string(),
            ));
        }
        let vk_target = vk::make_api_version(0, 1, 1, 0);
        let app_info = vk::ApplicationInfo::default()
            .application_version(1)
            .engine_version(1)
            .api_version(vk_target);
        let instance_info = vk::InstanceCreateInfo::default().application_info(&app_info);

        // SAFETY: get_instance_proc_addr は ash が読み込んだ loader のもの。
        // create_info はこの場の借用で、呼び出しの間だけ生きていればよい。
        let raw_instance = unsafe {
            xr_instance.create_vulkan_instance(
                system,
                core::mem::transmute::<
                    vk::PFN_vkGetInstanceProcAddr,
                    unsafe extern "system" fn(
                        *const core::ffi::c_void,
                        *const core::ffi::c_char,
                    )
                        -> Option<unsafe extern "system" fn()>,
                >(entry.static_fn().get_instance_proc_addr),
                (&instance_info) as *const _ as *const _,
            )
        }
        .map_err(|e| openxr_failure("xrCreateVulkanInstanceKHR failed", e))?
        .map_err(|raw| {
            vulkan_failure(
                "the OpenXR runtime's vkCreateInstance failed",
                vk::Result::from_raw(raw),
            )
        })?;
        // SAFETY: raw_instance はランタイムが作った本物の VkInstance。
        let instance = unsafe {
            ash::Instance::load(
                entry.static_fn(),
                vk::Instance::from_raw(raw_instance as u64),
            )
        };

        // SAFETY: instance は直前にランタイムが作ったもの。
        let raw_physical =
            unsafe { xr_instance.vulkan_graphics_device(system, instance.handle().as_raw() as _) }
                .map_err(|e| {
                    // SAFETY: まだ device を作っていないので instance を壊して良い。
                    unsafe { instance.destroy_instance(None) };
                    openxr_failure("xrGetVulkanGraphicsDevice2KHR failed", e)
                })?;
        let physical_device = vk::PhysicalDevice::from_raw(raw_physical as u64);

        let queue_family_index =
            // SAFETY: physical_device はこの instance のもの。
            match unsafe { pick_graphics_queue(&instance, physical_device) } {
                Ok(index) => index,
                Err(e) => {
                    // SAFETY: 同上。
                    unsafe { instance.destroy_instance(None) };
                    return Err(e);
                }
            };

        let wanted = match present_dmabuf_extensions(&instance, physical_device) {
            Ok(wanted) => wanted,
            Err(e) => {
                // SAFETY: 同上。
                unsafe { instance.destroy_instance(None) };
                return Err(e);
            }
        };
        let names: Vec<*const core::ffi::c_char> = wanted.iter().map(|n| n.as_ptr()).collect();
        let priorities = [1.0f32];
        let queue_infos = [vk::DeviceQueueCreateInfo::default()
            .queue_family_index(queue_family_index)
            .queue_priorities(&priorities)];
        let device_info = vk::DeviceCreateInfo::default()
            .queue_create_infos(&queue_infos)
            .enabled_extension_names(&names);
        // SAFETY: 上と同じ理由。physical_device と create_info はこの場の値。
        let raw_device = unsafe {
            xr_instance.create_vulkan_device(
                system,
                core::mem::transmute::<
                    vk::PFN_vkGetInstanceProcAddr,
                    unsafe extern "system" fn(
                        *const core::ffi::c_void,
                        *const core::ffi::c_char,
                    )
                        -> Option<unsafe extern "system" fn()>,
                >(entry.static_fn().get_instance_proc_addr),
                physical_device.as_raw() as _,
                (&device_info) as *const _ as *const _,
            )
        }
        .map_err(|e| {
            // SAFETY: device はまだ無い。
            unsafe { instance.destroy_instance(None) };
            openxr_failure("xrCreateVulkanDeviceKHR failed", e)
        })?
        .map_err(|raw| {
            // SAFETY: device はまだ無い。
            unsafe { instance.destroy_instance(None) };
            vulkan_failure(
                "the OpenXR runtime's vkCreateDevice failed",
                vk::Result::from_raw(raw),
            )
        })?;
        // SAFETY: raw_device はランタイムが作った本物の VkDevice。
        let device = unsafe {
            ash::Device::load(instance.fp_v1_0(), vk::Device::from_raw(raw_device as u64))
        };
        // SAFETY: 組が揃っている。
        Ok(unsafe {
            Self::adopt(
                entry,
                instance,
                physical_device,
                device,
                queue_family_index,
                &wanted,
            )
        })
    }

    /// 作った instance / device を所有へ移す。
    ///
    /// # Safety
    ///
    /// `instance` / `physical_device` / `device` / `queue_family_index` は同じ組で
    /// あること。以後の破棄はこの型が持つ。
    unsafe fn adopt(
        entry: ash::Entry,
        instance: ash::Instance,
        physical_device: vk::PhysicalDevice,
        device: ash::Device,
        queue_family_index: u32,
        enabled: &[&'static core::ffi::CStr],
    ) -> Self {
        // SAFETY: device はこの組のもので、queue_family_index は要求済み。
        let queue = unsafe { device.get_device_queue(queue_family_index, 0) };
        // SAFETY: physical_device はこの instance のもの。
        let memory_properties =
            unsafe { instance.get_physical_device_memory_properties(physical_device) };
        // SAFETY: 同上。
        let properties = unsafe { instance.get_physical_device_properties(physical_device) };
        let dmabuf_ready = DMABUF_DEVICE_EXTENSIONS.iter().all(|n| enabled.contains(n));
        let external_memory_fd =
            dmabuf_ready.then(|| ash::khr::external_memory_fd::Device::new(&instance, &device));
        let drm_format_modifier = dmabuf_ready
            .then(|| ash::ext::image_drm_format_modifier::Device::new(&instance, &device));
        // SAFETY: 同上。数えるだけ。
        let advertised = unsafe { instance.enumerate_device_extension_properties(physical_device) }
            .map(|v| v.len())
            .unwrap_or(0);
        let facts = VulkanDeviceFacts {
            device_name: device_name(&properties),
            api_version: format!(
                "{}.{}.{}",
                vk::api_version_major(properties.api_version),
                vk::api_version_minor(properties.api_version),
                vk::api_version_patch(properties.api_version),
            ),
            driver_version: properties.driver_version,
            queue_family_index,
            advertised_device_extension_count: advertised,
            dmabuf_extensions_present: dmabuf_ready,
            enabled_device_extensions: enabled
                .iter()
                .map(|n| n.to_string_lossy().into_owned())
                .collect(),
        };
        Self {
            entry,
            instance,
            physical_device,
            device,
            queue,
            queue_family_index,
            memory_properties,
            external_memory_fd,
            drm_format_modifier,
            facts,
        }
    }

    /// 観測できた事実。
    pub const fn facts(&self) -> &VulkanDeviceFacts {
        &self.facts
    }

    /// ash の logical device。
    pub const fn device(&self) -> &ash::Device {
        &self.device
    }

    /// ash の instance。
    pub const fn instance(&self) -> &ash::Instance {
        &self.instance
    }

    /// ash の entry (loader)。
    pub const fn entry(&self) -> &ash::Entry {
        &self.entry
    }

    /// 物理デバイス。
    pub const fn physical_device(&self) -> vk::PhysicalDevice {
        self.physical_device
    }

    /// graphics queue。
    pub const fn queue(&self) -> vk::Queue {
        self.queue
    }

    /// graphics queue family の番号。
    pub const fn queue_family_index(&self) -> u32 {
        self.queue_family_index
    }

    /// `VK_KHR_external_memory_fd` の面。無い構成では `None`。
    pub const fn external_memory_fd(&self) -> Option<&ash::khr::external_memory_fd::Device> {
        self.external_memory_fd.as_ref()
    }

    /// `VK_EXT_image_drm_format_modifier` の面。無い構成では `None`。
    pub const fn drm_format_modifier(
        &self,
    ) -> Option<&ash::ext::image_drm_format_modifier::Device> {
        self.drm_format_modifier.as_ref()
    }

    /// dmabuf の import 経路が組めるか。
    pub const fn supports_dmabuf_import(&self) -> bool {
        self.facts.dmabuf_extensions_present
    }

    /// 要求を満たす memory type の番号を探す。
    ///
    /// `allowed` は `VkMemoryRequirements::memoryTypeBits` と、外から来た
    /// 制約 (`vkGetMemoryFdPropertiesKHR` の `memoryTypeBits` など) の積を渡す。
    pub fn find_memory_type(&self, allowed: u32, flags: vk::MemoryPropertyFlags) -> Result<u32> {
        let count = self.memory_properties.memory_type_count as usize;
        for index in 0..count {
            let bit = 1u32 << index;
            if allowed & bit == 0 {
                continue;
            }
            if self.memory_properties.memory_types[index]
                .property_flags
                .contains(flags)
            {
                return Ok(index as u32);
            }
        }
        Err(Error::new(
            ErrorCode::Unsupported,
            "no Vulkan memory type satisfies the requested constraints",
            TraceId::unattributed(),
        )
        .with_detail("allowed_bits", i64::from(allowed))
        .with_detail("required_flags", flags.as_raw() as i64))
    }

    /// device の作業が終わるまで待つ。失敗を封筒で見たい呼び手のための口。
    ///
    /// `Drop` は失敗を報告できないので、`house.resource_lifecycle.release_paths`
    /// の「成功と失敗の両方の道で返す」を満たすためにここを別に開けてある。
    pub fn wait_idle(&self) -> Result<()> {
        // SAFETY: device はこの型が所有しているもの。
        unsafe { self.device.device_wait_idle() }
            .map_err(|e| vulkan_failure("vkDeviceWaitIdle failed", e))
    }
}

impl Drop for VulkanContext {
    fn drop(&mut self) {
        // SAFETY: この型が所有した device / instance を、他に生きた子が無い状態で
        // 一度だけ壊す。待ちの失敗は Drop からは報告できないので黙って進む
        // (報告経路は wait_idle)。
        unsafe {
            let _ = self.device.device_wait_idle();
            self.device.destroy_device(None);
            self.instance.destroy_instance(None);
        }
    }
}

/// Vulkan の loader を実行時に開く。
fn load_entry() -> Result<ash::Entry> {
    // SAFETY: ash が libvulkan.so.1 を dlopen する。開けなければ Err が返る。
    unsafe { ash::Entry::load() }.map_err(|e| {
        Error::new(
            ErrorCode::CapabilityUnavailable,
            "the Vulkan loader could not be opened at run time",
            TraceId::unattributed(),
        )
        .caused_by(e)
    })
}

/// graphics queue を持つ物理デバイスを選ぶ。
fn pick_physical_device(instance: &ash::Instance) -> Result<(vk::PhysicalDevice, u32)> {
    // SAFETY: instance は呼び手が作った生きたもの。
    let devices = unsafe { instance.enumerate_physical_devices() }
        .map_err(|e| vulkan_failure("vkEnumeratePhysicalDevices failed", e))?;
    let mut fallback = None;
    for device in devices {
        // SAFETY: device はこの instance が並べたもの。
        let Ok(index) = (unsafe { pick_graphics_queue(instance, device) }) else {
            continue;
        };
        // SAFETY: 同上。
        let properties = unsafe { instance.get_physical_device_properties(device) };
        // dmabuf の経路が要る以上、その拡張を持つ物を優先する。
        let ready = present_dmabuf_extensions(instance, device)
            .map(|enabled| DMABUF_DEVICE_EXTENSIONS.iter().all(|n| enabled.contains(n)))
            .unwrap_or(false);
        if ready && properties.device_type == vk::PhysicalDeviceType::DISCRETE_GPU {
            return Ok((device, index));
        }
        if fallback.is_none() || ready {
            fallback = Some((device, index));
        }
    }
    fallback.ok_or_else(|| {
        Error::new(
            ErrorCode::CapabilityUnavailable,
            "no Vulkan physical device with a graphics queue was found",
            TraceId::unattributed(),
        )
    })
}

/// graphics を持つ queue family を選ぶ。
///
/// # Safety
///
/// `physical_device` は `instance` が並べたものであること。
unsafe fn pick_graphics_queue(
    instance: &ash::Instance,
    physical_device: vk::PhysicalDevice,
) -> Result<u32> {
    // SAFETY: 呼び手の契約により組が揃っている。
    let families = unsafe { instance.get_physical_device_queue_family_properties(physical_device) };
    families
        .into_iter()
        .enumerate()
        .find(|(_, info)| info.queue_flags.contains(vk::QueueFlags::GRAPHICS))
        .map(|(index, _)| index as u32)
        .ok_or_else(|| {
            Error::new(
                ErrorCode::CapabilityUnavailable,
                "the Vulkan physical device has no graphics queue family",
                TraceId::unattributed(),
            )
        })
}

/// dmabuf 経路に要る拡張のうち、その物理デバイスが並べたものを返す。
///
/// 一つでも欠けていれば空を返す。**半端に有効化しない。** 欠けた構成では
/// import を作らず shm の退路だけを残すのが正しく、半分だけ有効化された device で
/// 後から落ちるのは避ける。
fn present_dmabuf_extensions(
    instance: &ash::Instance,
    physical_device: vk::PhysicalDevice,
) -> Result<Vec<&'static core::ffi::CStr>> {
    // SAFETY: physical_device は呼び手が同じ instance から引いたもの。
    let advertised = unsafe { instance.enumerate_device_extension_properties(physical_device) }
        .map_err(|e| vulkan_failure("vkEnumerateDeviceExtensionProperties failed", e))?;
    let names: Vec<String> = advertised
        .iter()
        .filter_map(|p| p.extension_name_as_c_str().ok())
        .map(|n| n.to_string_lossy().into_owned())
        .collect();
    let all_present = DMABUF_DEVICE_EXTENSIONS
        .iter()
        .all(|wanted| names.iter().any(|n| n == &wanted.to_string_lossy()));
    if all_present {
        Ok(DMABUF_DEVICE_EXTENSIONS.to_vec())
    } else {
        Ok(Vec::new())
    }
}

/// `VkPhysicalDeviceProperties::deviceName` を Rust の文字列へ。
fn device_name(properties: &vk::PhysicalDeviceProperties) -> String {
    properties
        .device_name_as_c_str()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|_| String::from("<unnamed>"))
}

/// 0 を含まない文字列を `CString` へ。
fn cstring(value: &str) -> Result<std::ffi::CString> {
    std::ffi::CString::new(value).map_err(|e| {
        Error::new(
            ErrorCode::InvalidArgument,
            "a name passed to Vulkan contained an interior NUL byte",
            TraceId::unattributed(),
        )
        .caused_by(e)
    })
}

/// Vulkan の失敗を封筒へ。
///
/// `code.error.no_raw_stack` により、原因は `source` に保つだけで封筒へは出さない。
pub(crate) fn vulkan_failure(message: &str, result: vk::Result) -> Error {
    Error::new(ErrorCode::HostRefused, message, TraceId::unattributed())
        .with_detail("vk_result", result.as_raw() as i64)
        .with_detail("vk_result_name", format!("{result:?}"))
}

/// OpenXR の失敗を封筒へ。
pub(crate) fn openxr_failure(message: &str, result: openxr::sys::Result) -> Error {
    Error::new(ErrorCode::HostRefused, message, TraceId::unattributed())
        .with_detail("xr_result", result.into_raw() as i64)
        .with_detail("xr_result_name", format!("{result:?}"))
}
