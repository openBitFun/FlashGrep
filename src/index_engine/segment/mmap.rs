use memmap2::Mmap;

pub(crate) fn advise_mmap_sequential(mmap: &Mmap) {
    #[cfg(target_family = "unix")]
    unsafe {
        if mmap.is_empty() {
            return;
        }
        let ptr = mmap.as_ptr() as *mut libc::c_void;
        let len = mmap.len();
        let _ = libc::posix_madvise(ptr, len, libc::POSIX_MADV_SEQUENTIAL);
    }

    #[cfg(not(target_family = "unix"))]
    let _ = mmap;
}

pub(crate) fn advise_mmap_random(mmap: &Mmap) {
    #[cfg(target_family = "unix")]
    unsafe {
        if mmap.is_empty() {
            return;
        }
        let ptr = mmap.as_ptr() as *mut libc::c_void;
        let len = mmap.len();
        let _ = libc::posix_madvise(ptr, len, libc::POSIX_MADV_RANDOM);
    }

    #[cfg(not(target_family = "unix"))]
    let _ = mmap;
}
