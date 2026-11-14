//! Exact-build FPP camera component owned by the formal PlayerPuppet: the simulation-side copy
//! of the camera the renderer uploads. Read together with the actor root, it tells which
//! simulation state a Present rendered (`depth_lab::cp2077_pose_join`).
//! Named layouts: RED4ext.SDK ad7277714ad30d6885d7050c5ba24fa0102f6920:
//! entEntity.hpp (components A0), entIComponent.hpp (owner 50),
//! entIPlacedComponent.hpp (worldTransform E0),
//! Generated/game/FPPCameraComponent.hpp (RTTI name and size 510).
//! Only the declared component array is enumerated; no pointer-word or address-space scan.

use std::collections::BTreeMap;

use windows::Win32::System::{
    LibraryLoader::GetModuleHandleW,
    ProcessStatus::{K32GetModuleInformation, MODULEINFO},
    Threading::GetCurrentProcess,
};

use crate::cp2077_pose_wiring::{
    FormalPlayerAddresses, RegionMemo, audit_formal_player, native_class_chain,
    read_process_value_no_query as rpm,
};

#[cfg(all(feature = "cp2077-diagnostics", debug_assertions))]
pub(crate) const CHAIN_ID: &str =
    "cp2077-20383525-player-components-rtti-fpp-world-transform-sdk-ad727771/v1";
const COMPONENTS: usize = 0xa0;
const NATIVE_TYPE: usize = 0x30;
const OWNER: usize = 0x50;
const TRANSFORM: usize = 0xe0;
const MAX_COMPONENTS: u32 = 512;

fn cname(value: &str) -> u64 {
    value.bytes().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x100_0000_01b3)
    })
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ArrayHeader {
    entries: usize,
    capacity: u32,
    size: u32,
}

pub(crate) fn address(base: usize, offset: usize) -> Result<usize, String> {
    base.checked_add(offset)
        .filter(|a| *a >= 0x10000)
        .ok_or_else(|| "camera address overflow".into())
}

fn valid_header(header: ArrayHeader) -> bool {
    header.entries >= 0x10000
        && header.entries.is_multiple_of(8)
        && header.size > 0
        && header.size <= header.capacity
        && header.capacity <= MAX_COMPONENTS
}

/// Retained addresses of the one owned FPP camera component. Never dereferenced: every access
/// is a fallible ReadProcessMemory, and `check_identity` re-reads the whole chain.
#[derive(Clone, Copy)]
pub(crate) struct FppCameraChain {
    player: FormalPlayerAddresses,
    header: ArrayHeader,
    index: u32,
    component: usize,
    native_type: usize,
    vtable: usize,
}

impl FppCameraChain {
    /// Control-thread resolution: may use VirtualQuery through the formal player audit.
    pub(crate) fn locate(player: FormalPlayerAddresses) -> Result<Self, String> {
        audit_formal_player(player)?;
        let module = unsafe { GetModuleHandleW(None) }.map_err(|_| "camera module unavailable")?;
        let mut info = MODULEINFO::default();
        if !unsafe {
            K32GetModuleInformation(
                GetCurrentProcess(),
                module,
                &mut info,
                std::mem::size_of::<MODULEINFO>() as u32,
            )
        }
        .as_bool()
        {
            return Err("camera module extent unavailable".into());
        }
        let start = info.lpBaseOfDll as usize;
        let end = start
            .checked_add(info.SizeOfImage as usize)
            .ok_or("camera module extent overflow")?;
        let header_address = address(player.player_object, COMPONENTS)?;
        let header = rpm::<ArrayHeader>(header_address)?;
        if !valid_header(header) {
            return Err("player components array is missing or exceeds the declared bound".into());
        }
        let mut candidates = BTreeMap::new();
        // 声明的组件数组最多 512 项，每项都要走一条 RTTI 父链。这些 CClass 描述符成批落在
        // 同一段内存里，共用一次 region 裁定；组件本身仍逐个 RPM 读、逐个验归属。
        let mut memo = RegionMemo::new();
        for index in 0..header.size {
            // DynArray<Handle<IComponent>>: 16-byte handle, object is its first word.
            let component = rpm::<usize>(address(header.entries, index as usize * 16)?)?;
            if component == 0 {
                continue;
            }
            let Some((chain, bytes)) = native_class_chain(&mut memo, component, start, end) else {
                continue;
            };
            if !(0x510..=0x10000).contains(&bytes)
                || !chain.contains(&cname("gameFPPCameraComponent"))
                || !chain.contains(&cname("entBaseCameraComponent"))
                || !chain.contains(&cname("entIPlacedComponent"))
            {
                continue;
            }
            if rpm::<usize>(address(component, OWNER)?)? == player.player_object {
                candidates.entry(component).or_insert(index);
            }
        }
        if candidates.len() != 1 {
            return Err(format!(
                "owned RTTI FPP camera candidate count is {}, expected one",
                candidates.len()
            ));
        }
        if rpm::<ArrayHeader>(header_address)? != header {
            return Err("player components changed during camera resolution".into());
        }
        let (component, index) = candidates
            .into_iter()
            .next()
            .ok_or("camera candidate disappeared")?;
        let chain = Self {
            player,
            header,
            index,
            component,
            native_type: rpm::<usize>(address(component, NATIVE_TYPE)?)?,
            vtable: rpm::<usize>(component)?,
        };
        chain.check_identity()?;
        Ok(chain)
    }

    /// For the diagnostic FOV/zoom fields, which production never reads.
    #[cfg(all(feature = "cp2077-diagnostics", debug_assertions))]
    pub(crate) fn component(&self) -> usize {
        self.component
    }

    pub(crate) fn check_identity(&self) -> Result<(), String> {
        if rpm::<usize>(self.player.player_slot as usize)? != self.player.player_object
            || rpm::<ArrayHeader>(address(self.player.player_object, COMPONENTS)?)? != self.header
            || rpm::<usize>(address(self.header.entries, self.index as usize * 16)?)?
                != self.component
            || rpm::<usize>(address(self.component, OWNER)?)? != self.player.player_object
            || rpm::<usize>(address(self.component, NATIVE_TYPE)?)? != self.native_type
            || rpm::<usize>(self.component)? != self.vtable
        {
            return Err(
                "FPP camera chain changed; no stale sample or automatic replacement".into(),
            );
        }
        Ok(())
    }

    /// One fallible read of the native WorldTransform words, with no identity check.
    pub(crate) fn read_transform(&self) -> Result<[u32; 8], String> {
        rpm(address(self.component, TRANSFORM)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pinned_sdk_array_layout_is_not_an_inferred_pointer_sequence() {
        assert_eq!(std::mem::size_of::<ArrayHeader>(), 16);
        assert_eq!(std::mem::offset_of!(ArrayHeader, entries), 0);
        assert_eq!(std::mem::offset_of!(ArrayHeader, capacity), 8);
        assert_eq!(std::mem::offset_of!(ArrayHeader, size), 12);
        assert_ne!(
            cname("gameFPPCameraComponent"),
            cname("gameCameraComponent")
        );
    }

    #[test]
    fn declared_array_bounds_do_not_turn_into_pointer_scans() {
        assert!(valid_header(ArrayHeader {
            entries: 0x10000,
            capacity: 10,
            size: 2
        }));
        assert!(!valid_header(ArrayHeader {
            entries: 0x10000,
            capacity: 513,
            size: 1
        }));
        assert!(!valid_header(ArrayHeader {
            entries: 0x10000,
            capacity: 1,
            size: 2
        }));
        assert!(!valid_header(ArrayHeader {
            entries: 0,
            capacity: 1,
            size: 1
        }));
        assert!(address(usize::MAX, 16).is_err());
    }

    #[test]
    fn rpm_identity_guard_rejects_owner_handle_array_and_player_changes() {
        // Synthetic buffers exercise fallible RPM and retained-chain guards, not RTTI discovery.
        let mut player_words = [0usize; 24];
        let mut camera_words = [0usize; 32];
        let player_object = player_words.as_ptr() as usize;
        let component = camera_words.as_ptr() as usize;
        let mut player_slot = player_object;
        let mut handles = [component, 0usize];
        let header = ArrayHeader {
            entries: handles.as_ptr() as usize,
            capacity: 1,
            size: 1,
        };
        player_words[COMPONENTS / 8] = header.entries;
        player_words[COMPONENTS / 8 + 1] = 1 | (1usize << 32);
        camera_words[OWNER / 8] = player_object;
        camera_words[NATIVE_TYPE / 8] = 0x123400;
        camera_words[0] = 0x567800;
        camera_words[TRANSFORM / 8] = 0x0000_0002_0000_0001;
        let chain = FppCameraChain {
            player: FormalPlayerAddresses {
                system_object: 0,
                player_slot: &player_slot,
                player_object,
                transform_component_slot: std::ptr::null(),
                transform_component: 0,
                component_owner_slot: std::ptr::null(),
                world_transform: std::ptr::null(),
            },
            header,
            index: 0,
            component,
            native_type: 0x123400,
            vtable: 0x567800,
        };
        assert!(chain.check_identity().is_ok());
        assert_eq!(chain.read_transform().unwrap()[..2], [1, 2]);
        camera_words[OWNER / 8] = 0;
        std::hint::black_box(&camera_words);
        assert!(chain.check_identity().is_err());
        camera_words[OWNER / 8] = player_object;
        handles[0] = 0;
        std::hint::black_box((&camera_words, &handles));
        assert!(chain.check_identity().is_err());
        handles[0] = component;
        player_words[COMPONENTS / 8 + 1] = 1;
        std::hint::black_box((&handles, &player_words));
        assert!(chain.check_identity().is_err());
        player_words[COMPONENTS / 8 + 1] = 1 | (1usize << 32);
        player_slot = 0;
        std::hint::black_box((&player_words, &player_slot));
        assert!(chain.check_identity().is_err());
    }
}
