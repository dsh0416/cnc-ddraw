//! Audited COM raw-pointer helpers for optional inputs, uninitialized outputs, and interface out-params.
use std::mem::MaybeUninit;
use std::ops::Deref;

use windows::core::{IUnknown, Interface, OutRef, Ref, Result, HRESULT};
use windows::Win32::Foundation::{CLASS_E_NOAGGREGATION, E_POINTER};

/// Wrapper allowing a COM interface pointer to cross thread boundaries inside a mutex.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct SendCom<T: Interface>(pub(crate) T);

// SAFETY: Our DirectDraw COM objects (`Palette`, `Clipper`, `Surface`) use atomic refcounting
// and internal mutexes; user `IUnknown` private data is held under `SurfaceSlot`'s mutex and
// only accessed or dropped exclusively.
unsafe impl<T: Interface> Send for SendCom<T> {}

impl<T: Interface> Deref for SendCom<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// Converts a nullable COM input pointer into an optional shared reference.
///
/// # Safety
/// If non-null, `ptr` must be aligned, point to a valid initialized `T`, and remain immutable for `'a`.
pub(crate) unsafe fn opt_ref<'a, T>(ptr: *const T) -> Option<&'a T> {
    // SAFETY: Caller guarantees `ptr` is null or points to a valid initialized `T` for `'a`.
    unsafe { ptr.as_ref() }
}

/// Copies a nullable COM input pointer by value.
///
/// # Safety
/// If non-null, `ptr` must be aligned and point to a valid initialized `T`.
pub(crate) unsafe fn opt_copy<T: Copy>(ptr: *const T) -> Option<T> {
    // SAFETY: Caller guarantees `ptr` is null or points to a valid initialized `T`.
    unsafe { ptr.as_ref() }.copied()
}

/// Converts a required COM input pointer into a shared reference, returning `err` if null.
///
/// # Safety
/// If non-null, `ptr` must be aligned, point to a valid initialized `T`, and remain immutable for `'a`.
pub(crate) unsafe fn req_ref<'a, T>(ptr: *const T, err: HRESULT) -> Result<&'a T> {
    // SAFETY: Caller guarantees `ptr` is null or points to a valid initialized `T` for `'a`.
    unsafe { ptr.as_ref() }.ok_or_else(|| err.into())
}

/// Converts a required COM output pointer into a mutable `MaybeUninit<T>`, returning `err` if null.
///
/// # Safety
/// If non-null, `ptr` must be aligned and exclusively writable for a `T` for `'a`.
pub(crate) unsafe fn out_slot<'a, T>(ptr: *mut T, err: HRESULT) -> Result<&'a mut MaybeUninit<T>> {
    // SAFETY: Caller guarantees `ptr` is null or aligned and exclusively writable for `'a`.
    unsafe { ptr.cast::<MaybeUninit<T>>().as_mut() }.ok_or_else(|| err.into())
}

/// Writes `value` into a COM output pointer, returning `err` if null.
///
/// # Safety
/// If non-null, `ptr` must be aligned and exclusively writable for a `T`.
pub(crate) unsafe fn write_out<T>(ptr: *mut T, value: T, err: HRESULT) -> Result<()> {
    // SAFETY: Caller guarantees `ptr` is null or aligned and exclusively writable.
    let slot = unsafe { out_slot(ptr, err)? };
    slot.write(value);
    Ok(())
}

/// Writes a COM interface output parameter, always nulling it on error per COM rules.
pub(crate) fn put_interface<T>(out: OutRef<T>, make: impl FnOnce() -> Result<T>) -> Result<()>
where
    T: Interface + windows_core::Type<T, Default = Option<T>>,
{
    if out.is_null() {
        return Err(E_POINTER.into());
    }
    match make() {
        Ok(value) => out.write(Some(value)),
        Err(error) => {
            out.write(None)?;
            Err(error)
        }
    }
}

/// Rejects COM aggregation (`pUnkOuter != NULL`) with `CLASS_E_NOAGGREGATION`.
pub(crate) fn no_aggregation(outer: Ref<IUnknown>) -> Result<()> {
    if outer.is_null() {
        Ok(())
    } else {
        Err(CLASS_E_NOAGGREGATION.into())
    }
}
