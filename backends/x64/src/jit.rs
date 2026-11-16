//! Executable memory for JIT code, W^X: the pages are mapped read+write,
//! filled, then switched to read+execute and never writable again.

use std::io;

pub struct ExecMem {
    ptr: *mut u8,
    len: usize,
}

// The mapping is immutable after construction.
unsafe impl Send for ExecMem {}
unsafe impl Sync for ExecMem {}

impl ExecMem {
    /// Copy `code` into fresh pages and make them executable.
    pub fn new(code: &[u8]) -> io::Result<ExecMem> {
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
        let len = code.len().max(1).div_ceil(page) * page;
        let ptr = unsafe {
            libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_PRIVATE | libc::MAP_ANONYMOUS, -1, 0)
        };
        if ptr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let ptr = ptr as *mut u8;
        unsafe {
            std::ptr::copy_nonoverlapping(code.as_ptr(), ptr, code.len());
            // Fill the tail with int3 so a stray jump traps instead of sliding.
            std::ptr::write_bytes(ptr.add(code.len()), 0xCC, len - code.len());
        }
        if unsafe { libc::mprotect(ptr as *mut libc::c_void, len, libc::PROT_READ | libc::PROT_EXEC) } != 0 {
            let e = io::Error::last_os_error();
            unsafe { libc::munmap(ptr as *mut libc::c_void, len) };
            return Err(e);
        }
        Ok(ExecMem { ptr, len })
    }

    pub fn ptr(&self) -> *const u8 {
        self.ptr
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl Drop for ExecMem {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.ptr as *mut libc::c_void, self.len) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::enc::*;

    #[test]
    fn run_generated_code() {
        // extern "C" fn(a: i64, b: i64) -> i64 { a * b + 7 }
        let mut a = Asm::new();
        a.mov_rr(RAX, RDI);
        a.imul(RAX, RSI);
        a.alu_imm(Alu::Add, Sz::Q, RAX, 7);
        a.ret();
        a.finish().unwrap();
        let m = ExecMem::new(&a.buf).unwrap();
        let f: extern "C" fn(i64, i64) -> i64 = unsafe { std::mem::transmute(m.ptr()) };
        assert_eq!(f(6, 7), 49);
        assert_eq!(f(-3, 5), -8);
    }

    #[test]
    fn pages_are_not_writable() {
        let m = ExecMem::new(&[0xC3]).unwrap();
        // Ask the kernel: /proc/self/maps lists the mapping as r-x.
        let maps = std::fs::read_to_string("/proc/self/maps").unwrap();
        let addr = m.ptr() as usize;
        let line = maps
            .lines()
            .find(|l| {
                let (r, _) = l.split_once(' ').unwrap();
                let (s, e) = r.split_once('-').unwrap();
                let (s, e) = (usize::from_str_radix(s, 16).unwrap(), usize::from_str_radix(e, 16).unwrap());
                s <= addr && addr < e
            })
            .unwrap();
        assert!(line.split_whitespace().nth(1).unwrap().starts_with("r-x"), "{line}");
    }
}
