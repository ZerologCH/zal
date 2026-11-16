//! Addendum v2 runtime-helper contract (RFC 0002). These symbol names and
//! signatures are the interface stream R is implementing; codegen switches the
//! hot ops onto them once they are on `main` (resolved for JIT through an
//! extended `runtime_symbols()`, and as `R_X86_64_PLT32` calls for AOT). Kept
//! here so the exact names live next to the code that will call them and main
//! keeps building until then.
//!
//! All helpers use the C ABI, take `ZalValue*` pointing into the operand stack
//! or locals, and (except the two `void` ones) return 0 on success or a
//! nonzero "not applicable — take the generic path" code, per the addendum.
#![allow(dead_code)]

/// `u32 ZAL_SLICE`: the interpreter's preemption slice. On a taken backward
/// branch, inline code does `ctx->slice += 1; if ctx->slice >= ZAL_SLICE { r =
/// zal_rt_tick(ctx, target); ... }` instead of calling `zal_rt_tick` every time.
pub const SYM_SLICE: &str = "ZAL_SLICE";

/// `int64_t zal_rt_len(const ZalValue *v)` — length of List/Map/Str, or -1.
pub const SYM_LEN: &str = "zal_rt_len";
/// `uint32_t zal_rt_index(ZalValue *dst, const ZalValue *list, int64_t i)`.
pub const SYM_INDEX: &str = "zal_rt_index";
/// `uint32_t zal_rt_for_next(ZalValue *slot3)` — locals [list, index, var].
pub const SYM_FOR_NEXT: &str = "zal_rt_for_next";
/// `uint32_t zal_rt_upd(ZalValue *local, ZalValue *args, uint32_t builtin)`.
pub const SYM_UPD: &str = "zal_rt_upd";
/// `uint32_t zal_rt_tag(const ZalValue *v)` — variant tag / Bool, 0xFFFF if not a variant.
pub const SYM_TAG: &str = "zal_rt_tag";
/// `uint32_t zal_rt_payload(ZalValue *dst, const ZalValue *v)`.
pub const SYM_PAYLOAD: &str = "zal_rt_payload";
/// `uint32_t zal_rt_field(ZalValue *dst, const ZalValue *rec, uint32_t i)`.
pub const SYM_FIELD: &str = "zal_rt_field";
/// `uint32_t zal_rt_map_get(ZalValue *dst, const ZalValue *map, const ZalValue *key)`.
pub const SYM_MAP_GET: &str = "zal_rt_map_get";
/// `void zal_rt_const(ZalValue *dst, uint32_t const_index, const ZalCtx*)`.
pub const SYM_CONST: &str = "zal_rt_const";
/// `uint32_t zal_rt_concat(ZalCtx*, uint32_t n)` — ConcatN of the top n values.
pub const SYM_CONCAT: &str = "zal_rt_concat";
/// `uint32_t zal_rt_call(ZalCtx*, uint32_t pc, uint32_t callee, uint32_t argc)`.
pub const SYM_CALL: &str = "zal_rt_call";

/// Hot ops to move onto the helpers once they land (addendum work list):
/// UpdLocal, IndexLL/Index, ForNext, LenLocal, JumpIfNotTag/TagIs, Payload/
/// LoadPayload/MovePayload, FieldGet/LoadField, MapGetJump, Const(Str),
/// ConcatN, Call/Ret (via `zal_rt_call` + native→native dispatch), and
/// back-edge ticks (inline `ZAL_SLICE` counting around `zal_rt_tick`).
pub const _WORKLIST: () = ();
