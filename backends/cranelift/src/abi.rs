//! Runtime ABI (RFC 0002) as defined by `zal-vm::native`, plus the forms the
//! code generator needs (Cranelift wants `i32` memory offsets).

pub use zal_vm::native::{
    data_layout, runtime_symbols, tag_is_plain, DataLayout, NativeFn, TAG_LIST, TAG_MAP, TAG_RECORD, TAG_TASK, TAG_TUPLE, TAG_VARIANT, ZalCtx, ZalValue, TAG_BOOL, TAG_DUR, TAG_INT, TAG_STR, TAG_UNIT,
    ZAL_EXIT_CALL as EXIT_CALL, ZAL_EXIT_DONE as EXIT_DONE, ZAL_EXIT_RET as EXIT_RET,
    ZAL_EXIT_SUSPEND as EXIT_SUSPEND, ZAL_EXIT_TRAP as EXIT_TRAP, ZAL_JUMP as JUMP, ZAL_TRAP_ADD as TRAP_ADD,
    ZAL_TRAP_DIV as TRAP_DIV, ZAL_TRAP_MOD as TRAP_REM, ZAL_TRAP_MUL as TRAP_MUL, ZAL_TRAP_SUB as TRAP_SUB,
};

pub(crate) const CTX_STACK: i32 = zal_vm::native::CTX_OFF_STACK as i32;
pub(crate) const CTX_SP: i32 = zal_vm::native::CTX_OFF_SP as i32;
pub(crate) const CTX_BASE: i32 = zal_vm::native::CTX_OFF_BASE as i32;
pub(crate) const CTX_PC: i32 = zal_vm::native::CTX_OFF_PC as i32;
pub(crate) const CTX_SLICE: i32 = zal_vm::native::CTX_OFF_SLICE as i32;
pub(crate) const VALUE_SIZE: i64 = zal_vm::native::VALUE_SIZE as i64;
pub(crate) const VAL_PAYLOAD: i32 = zal_vm::native::VALUE_PAYLOAD_OFFSET as i32;

// The generated code hard-wires these facts; fail the build if zal-vm moves them.
const _: () = {
    assert!(VALUE_SIZE == 24 && VAL_PAYLOAD == 8);
    assert!(CTX_SP == CTX_STACK + 8 && CTX_BASE == CTX_SP + 8);
    // `is_plain` in codegen tests bit `tag` of PLAIN_MASK.
    let mut t = 0u8;
    while t < 32 {
        assert!(tag_is_plain(t) == ((PLAIN_MASK >> t) & 1 == 1));
        t += 1;
    }
};

/// Bit set of the tags whose payload is an `Rc` pointer to the allocation
/// header, strong count first (Str, List, Map; RFC 0002 §3).
pub(crate) const RC_MASK: u32 = (1 << TAG_STR) | (1 << zal_vm::native::TAG_LIST) | (1 << zal_vm::native::TAG_MAP);

/// Bit set of the plain tags (Unit, Int, Bool, Dur).
pub(crate) const PLAIN_MASK: u32 = (1 << TAG_UNIT) | (1 << TAG_INT) | (1 << TAG_BOOL) | (1 << TAG_DUR);

/// Runtime symbols the JIT links against: `(name, address)` for every
/// `zal_rt_*` entry point. Production code uses [`runtime_symbols`]; tests
/// may pass stubs.
pub type RtSymbols = [(&'static str, *const u8)];

/// Taken backward branches between `zal_rt_tick` calls (addendum v2 §1).
pub(crate) use zal_vm::native::ZAL_SLICE;

/// `zal_rt_tag` result for values that are neither a variant nor a Bool.
pub(crate) const TAG_NONE: u32 = 0xFFFF;
