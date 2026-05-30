//! Small safe-ish helpers over the CoreAudio `AudioObject*` property API.
//!
//! Every HAL read funnels through here so the unsafe FFI lives in one audited place. All functions are
//! `unsafe` to call because the caller must pass a `T` whose layout matches what the property returns.

use std::mem::{MaybeUninit, size_of};
use std::os::raw::c_void;
use std::ptr;

use anyhow::{Result, bail};
use coreaudio_sys as ca;

/// Build a property address.
pub(crate) fn address(selector: u32, scope: u32, element: u32) -> ca::AudioObjectPropertyAddress {
    ca::AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: scope,
        mElement: element,
    }
}

/// The common case: global scope, main element.
pub(crate) fn global(selector: u32) -> ca::AudioObjectPropertyAddress {
    address(
        selector,
        ca::kAudioObjectPropertyScopeGlobal,
        ca::kAudioObjectPropertyElementMain,
    )
}

/// Read a single fixed-size property value (`T` must match the property's data type and layout).
///
/// # Safety
/// `T`'s size and layout must equal the property's data type for `obj`/`addr`.
pub(crate) unsafe fn get<T: Copy>(
    obj: ca::AudioObjectID,
    addr: &ca::AudioObjectPropertyAddress,
) -> Result<T> {
    let mut size = size_of::<T>() as u32;
    let mut val = MaybeUninit::<T>::uninit();
    let st = unsafe {
        ca::AudioObjectGetPropertyData(
            obj,
            addr,
            0,
            ptr::null(),
            &mut size,
            val.as_mut_ptr() as *mut c_void,
        )
    };
    if st != 0 {
        bail!(
            "AudioObjectGetPropertyData(obj={obj}, selector={:#010x}) failed: OSStatus {st}",
            addr.mSelector
        );
    }
    Ok(unsafe { val.assume_init() })
}

/// Read a variable-length array property into a `Vec<T>`.
///
/// # Safety
/// `T`'s layout must equal the property's element type for `obj`/`addr`.
pub(crate) unsafe fn get_vec<T: Copy>(
    obj: ca::AudioObjectID,
    addr: &ca::AudioObjectPropertyAddress,
) -> Result<Vec<T>> {
    let mut size: u32 = 0;
    let st = unsafe { ca::AudioObjectGetPropertyDataSize(obj, addr, 0, ptr::null(), &mut size) };
    if st != 0 {
        bail!(
            "AudioObjectGetPropertyDataSize(obj={obj}, selector={:#010x}) failed: OSStatus {st}",
            addr.mSelector
        );
    }
    let count = size as usize / size_of::<T>();
    let mut buf: Vec<T> = Vec::with_capacity(count);
    if count > 0 {
        let st = unsafe {
            ca::AudioObjectGetPropertyData(
                obj,
                addr,
                0,
                ptr::null(),
                &mut size,
                buf.as_mut_ptr() as *mut c_void,
            )
        };
        if st != 0 {
            bail!(
                "AudioObjectGetPropertyData(obj={obj}, selector={:#010x}, array) failed: OSStatus {st}",
                addr.mSelector
            );
        }
        // SAFETY: the call above filled `count` contiguous `T`s.
        unsafe { buf.set_len(count) };
    }
    Ok(buf)
}
