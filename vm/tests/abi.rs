//! The native ABI layout (RFC 0002 §2-§3) is a contract with machine code:
//! these tests pin tags, payload offsets and the context layout.

use std::rc::Rc;

use zal_vm::native::*;
use zal_vm::value::ZMap;
use zal_vm::Value;

fn bytes(v: &Value) -> [u8; 24] {
    // SAFETY: Value is 24 bytes; reading its bytes (padding included) is only
    // used to inspect the tag and payload, which are always initialised.
    unsafe { std::mem::transmute_copy::<Value, [u8; 24]>(v) }
}

fn word(b: &[u8; 24], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

#[test]
fn value_is_24_bytes_tag_first_payload_at_8() {
    assert_eq!(std::mem::size_of::<Value>(), VALUE_SIZE);
    assert_eq!(std::mem::align_of::<Value>(), 8);
    let o = VALUE_PAYLOAD_OFFSET;

    assert_eq!(bytes(&Value::Unit)[0], TAG_UNIT);

    let b = bytes(&Value::Int(-0x1122_3344_5566_7788));
    assert_eq!(b[0], TAG_INT);
    assert_eq!(word(&b, o) as i64, -0x1122_3344_5566_7788);

    for (x, n) in [(false, 0u8), (true, 1u8)] {
        let b = bytes(&Value::Bool(x));
        assert_eq!(b[0], TAG_BOOL);
        assert_eq!(b[o], n);
    }

    let s: Rc<str> = Rc::from("hello");
    let b = bytes(&Value::Str(s.clone()));
    assert_eq!(b[0], TAG_STR);
    // Rc payloads point at the allocation (strong, weak counts, then data).
    assert_eq!(word(&b, o) + 16, Rc::as_ptr(&s) as *const u8 as u64);
    assert_eq!(word(&b, o + 8), 5);

    let b = bytes(&Value::Dur(1234));
    assert_eq!(b[0], TAG_DUR);
    assert_eq!(word(&b, o) as i64, 1234);

    let l = Rc::new(vec![Value::Int(1)]);
    let b = bytes(&Value::List(l.clone()));
    assert_eq!(b[0], TAG_LIST);
    assert_eq!(word(&b, o) + 16, Rc::as_ptr(&l) as u64);

    let m = Rc::new(ZMap::default());
    let b = bytes(&Value::Map(m.clone()));
    assert_eq!(b[0], TAG_MAP);
    assert_eq!(word(&b, o) + 16, Rc::as_ptr(&m) as u64);

    // Runtime-private variants follow in declaration order.
    assert_eq!(bytes(&Value::Tuple(Rc::from(vec![])))[0], 7);
    assert_eq!(bytes(&Value::Record(Rc::from(vec![])))[0], 8);
    assert_eq!(bytes(&Value::Variant(3, None))[0], 9);
    assert_eq!(bytes(&Value::Task(7))[0], 10);
}

#[test]
fn plain_tags_are_exactly_unit_int_bool_dur() {
    let plain: Vec<u8> = (0..=10).filter(|t| tag_is_plain(*t)).collect();
    assert_eq!(plain, vec![TAG_UNIT, TAG_INT, TAG_BOOL, TAG_DUR]);
}

#[test]
fn context_layout_matches_rfc() {
    assert_eq!(CTX_OFF_VM, 0);
    assert_eq!(CTX_OFF_STACK, 8);
    assert_eq!(CTX_OFF_SP, 16);
    assert_eq!(CTX_OFF_BASE, 24);
    assert_eq!(CTX_OFF_PC, 32);
    assert_eq!(CTX_OFF_FUNC, 36);
    assert_eq!(CTX_OFF_SLICE, 40);
    assert_eq!(std::mem::size_of::<ZalCtx>(), 48);
}

#[test]
fn exit_codes_match_rfc() {
    assert_eq!((ZAL_NEXT, ZAL_EXIT_CALL, ZAL_EXIT_RET, ZAL_EXIT_SUSPEND, ZAL_EXIT_TRAP, ZAL_EXIT_DONE, ZAL_JUMP), (0, 1, 2, 3, 4, 5, 6));
    assert_eq!((ZAL_TRAP_ADD, ZAL_TRAP_SUB, ZAL_TRAP_MUL, ZAL_TRAP_DIV, ZAL_TRAP_MOD), (1, 2, 3, 4, 5));
}

#[test]
fn clone_and_drop_keep_refcounts_balanced() {
    let l = Rc::new(vec![Value::Int(1), Value::Int(2)]);
    let src = Value::List(l.clone());
    let mut slot = std::mem::MaybeUninit::<Value>::uninit();
    unsafe {
        zal_rt_clone(slot.as_mut_ptr(), &src);
        assert_eq!(Rc::strong_count(&l), 3);
        assert_eq!(*slot.as_ptr(), src);
        zal_rt_drop(slot.as_mut_ptr());
    }
    assert_eq!(Rc::strong_count(&l), 2);
}
