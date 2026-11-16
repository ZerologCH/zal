//! Typed runtime helpers (RFC 0002 v2 §3): each one against the interpreter's
//! semantics for the op it serves, including the "not applicable" cases
//! (where the interpreter traps or the type is unexpected) and refcounts.

use std::mem::MaybeUninit;
use std::rc::Rc;

use zal_vm::host::{MockHost, PolicyBroker};
use zal_vm::native::*;
use zal_vm::value::{str_val, ZMap};
use zal_vm::{Value, Vm};

fn list(xs: &[i64]) -> Value {
    Value::List(Rc::new(xs.iter().map(|x| Value::Int(*x)).collect()))
}

fn map(kv: &[(&str, i64)]) -> Value {
    Value::Map(Rc::new(kv.iter().map(|(k, v)| (Rc::from(*k), Value::Int(*v))).collect::<ZMap>()))
}

unsafe fn take(slot: &mut MaybeUninit<Value>) -> Value {
    slot.as_ptr().read()
}

#[test]
fn len_matches_len_builtin() {
    unsafe {
        assert_eq!(zal_rt_len(&list(&[1, 2, 3])), 3);
        assert_eq!(zal_rt_len(&map(&[("a", 1)])), 1);
        assert_eq!(zal_rt_len(&str_val("häh")), 3); // chars, not bytes
        assert_eq!(zal_rt_len(&Value::Int(4)), -1);
        assert_eq!(zal_rt_len(&Value::Unit), -1);
    }
}

#[test]
fn index_clones_in_bounds_and_declines_otherwise() {
    unsafe {
        let l = list(&[10, 20]);
        let mut d = MaybeUninit::uninit();
        assert_eq!(zal_rt_index(d.as_mut_ptr(), &l, 1), 0);
        assert_eq!(take(&mut d), Value::Int(20));
        for i in [2, -1, i64::MIN, i64::MAX] {
            assert_eq!(zal_rt_index(d.as_mut_ptr(), &l, i), 1);
        }
        assert_eq!(zal_rt_index(d.as_mut_ptr(), &Value::Int(1), 0), 1);
        // dst aliasing the list consumes it (refcount back to 1).
        let inner = Rc::new(vec![str_val("x")]);
        let mut slot = Value::List(inner.clone());
        assert_eq!(Rc::strong_count(&inner), 2);
        assert_eq!(zal_rt_index(&mut slot, &slot as *const Value, 0), 0);
        assert_eq!(slot, str_val("x"));
        assert_eq!(Rc::strong_count(&inner), 1);
    }
}

#[test]
fn for_next_advances_like_the_for_loop_op() {
    unsafe {
        let mut s = [list(&[7, 8]), Value::Int(0), Value::Unit];
        assert_eq!(zal_rt_for_next(s.as_mut_ptr()), 0);
        assert_eq!((&s[1], &s[2]), (&Value::Int(1), &Value::Int(7)));
        assert_eq!(zal_rt_for_next(s.as_mut_ptr()), 0);
        assert_eq!((&s[1], &s[2]), (&Value::Int(2), &Value::Int(8)));
        assert_eq!(zal_rt_for_next(s.as_mut_ptr()), 1);
        assert_eq!(s[1], Value::Int(2)); // exhausted: unchanged
        // Negative index: the interpreter casts to usize, so it is exhausted.
        let mut s = [list(&[1]), Value::Int(-1), Value::Unit];
        assert_eq!(zal_rt_for_next(s.as_mut_ptr()), 1);
        let mut s = [Value::Int(0), Value::Int(0), Value::Unit];
        assert_eq!(zal_rt_for_next(s.as_mut_ptr()), 2);
        let mut s = [list(&[1]), Value::Bool(true), Value::Unit];
        assert_eq!(zal_rt_for_next(s.as_mut_ptr()), 2);
    }
}

#[test]
fn upd_matches_upd_local() {
    unsafe {
        let mut l = list(&[1, 2, 3]);
        let mut a = [Value::Int(4)];
        assert_eq!(zal_rt_upd(&mut l, a.as_mut_ptr(), ZAL_BUILTIN_PUSH), 0);
        std::mem::forget(a); // consumed
        assert_eq!(l, list(&[1, 2, 3, 4]));
        let mut a = [Value::Int(0)];
        assert_eq!(zal_rt_upd(&mut l, a.as_mut_ptr(), ZAL_BUILTIN_REMOVE), 0);
        assert_eq!(l, list(&[2, 3, 4]));
        let mut a = [Value::Int(1), Value::Int(9)];
        assert_eq!(zal_rt_upd(&mut l, a.as_mut_ptr(), ZAL_BUILTIN_SET), 0);
        std::mem::forget(a);
        assert_eq!(l, list(&[2, 9, 4]));
        // Out of range / wrong types: generic path (the interpreter traps), nothing changed.
        for (args, b) in [(vec![Value::Int(3)], ZAL_BUILTIN_REMOVE), (vec![Value::Int(-1), Value::Int(0)], ZAL_BUILTIN_SET), (vec![Value::Bool(true)], ZAL_BUILTIN_REMOVE)] {
            let mut args = args;
            assert_eq!(zal_rt_upd(&mut l, args.as_mut_ptr(), b), 2);
        }
        assert_eq!(l, list(&[2, 9, 4]));
        let mut m = map(&[("a", 1)]);
        let mut a = [str_val("b"), Value::Int(2)];
        assert_eq!(zal_rt_upd(&mut m, a.as_mut_ptr(), ZAL_BUILTIN_MAP_SET), 0);
        std::mem::forget(a);
        assert_eq!(m, map(&[("a", 1), ("b", 2)]));
        let mut a = [Value::Int(1), Value::Int(2)];
        assert_eq!(zal_rt_upd(&mut m, a.as_mut_ptr(), ZAL_BUILTIN_MAP_SET), 2);
        let mut a = [Value::Int(1)];
        assert_eq!(zal_rt_upd(&mut m, a.as_mut_ptr(), ZAL_BUILTIN_PUSH), 2);
        assert_eq!(zal_rt_upd(&mut l, a.as_mut_ptr(), 999), 2);
        // Copy-on-write: a shared list is copied, the other owner keeps its value.
        let shared = Rc::new(vec![Value::Int(1)]);
        let mut l2 = Value::List(shared.clone());
        let mut a = [Value::Int(2)];
        assert_eq!(zal_rt_upd(&mut l2, a.as_mut_ptr(), ZAL_BUILTIN_PUSH), 0);
        std::mem::forget(a);
        assert_eq!(*shared, vec![Value::Int(1)]);
        assert_eq!(l2, list(&[1, 2]));
    }
}

#[test]
fn tag_matches_tag_tests() {
    unsafe {
        assert_eq!(zal_rt_tag(&Value::Variant(3, None)), 3);
        assert_eq!(zal_rt_tag(&Value::Variant(1, Some(Rc::new(Value::Unit)))), 1);
        assert_eq!(zal_rt_tag(&Value::Bool(false)), 0);
        assert_eq!(zal_rt_tag(&Value::Bool(true)), 1);
        assert_eq!(zal_rt_tag(&Value::Int(1)), 0xFFFF);
    }
}

#[test]
fn payload_field_and_map_get() {
    unsafe {
        let mut d = MaybeUninit::uninit();
        let v = Value::Variant(1, Some(Rc::new(str_val("p"))));
        assert_eq!(zal_rt_payload(d.as_mut_ptr(), &v), 0);
        assert_eq!(take(&mut d), str_val("p"));
        assert_eq!(zal_rt_payload(d.as_mut_ptr(), &Value::Variant(0, None)), 2);
        assert_eq!(zal_rt_payload(d.as_mut_ptr(), &Value::Int(0)), 2);

        let r = Value::Record(Rc::from(vec![Value::Int(1), str_val("b")]));
        assert_eq!(zal_rt_field(d.as_mut_ptr(), &r, 1), 0);
        assert_eq!(take(&mut d), str_val("b"));
        assert_eq!(zal_rt_field(d.as_mut_ptr(), &r, 2), 2);
        assert_eq!(zal_rt_field(d.as_mut_ptr(), &Value::Tuple(Rc::from(vec![Value::Int(1)])), 0), 2);

        let m = map(&[("k", 5)]);
        assert_eq!(zal_rt_map_get(d.as_mut_ptr(), &m, &str_val("k")), 0);
        assert_eq!(take(&mut d), Value::Int(5));
        assert_eq!(zal_rt_map_get(d.as_mut_ptr(), &m, &str_val("x")), 1);
        assert_eq!(zal_rt_map_get(d.as_mut_ptr(), &m, &Value::Int(1)), 2);
        assert_eq!(zal_rt_map_get(d.as_mut_ptr(), &list(&[]), &str_val("k")), 2);
    }
}

/// zal_rt_const and zal_rt_concat need a running context: a body that uses
/// them for every Const/ConcatN must give the interpreter's result.
#[test]
fn const_and_concat_in_a_running_program() {
    let src = r#"module t
fn f(n: Int) -> Str {
  let mut s = "a" + "b"
  let mut xs: List<Int> = []
  for i in range(n) { xs = push(xs, i) }
  let mut m: Map<Int> = map_new()
  for x in xs { m = map_set(m, "k" + str(x), x * 2) }
  let mut t = 0
  for k in map_keys(m) { match map_get(m, k) { Some(v) => { t = t + v }, None => {} } }
  s + "-" + str(len(xs)) + "-" + str(t) + "-" + str(xs[n - 1])
}"#;
    let p = zal_compiler::compile_source(src, "t.zal").unwrap();
    let mut outs = vec![];
    for table in [None, Some(fake::table(&p)), Some(fake::table_generic(&p))] {
        let mut vm = Vm::new(&p, MockHost::default(), PolicyBroker::new(vec![]));
        if let Some(t) = table {
            vm.set_native(t);
        }
        outs.push(vm.run("f", vec![Value::Int(50)]).map(|v| v.to_string()).map_err(|e| e.to_string()));
    }
    assert_eq!(outs[0], Ok("ab-50-2450-49".to_string()));
    assert_eq!(outs[0], outs[1]);
    assert_eq!(outs[0], outs[2]);
}

#[test]
fn slice_constant_is_the_interpreters() {
    assert_eq!(ZAL_SLICE, 512);
    let names: Vec<&str> = runtime_symbols().iter().map(|s| s.0).collect();
    for n in ["zal_rt_op", "zal_rt_tick", "zal_rt_trap", "zal_rt_call", "zal_rt_ret", "zal_rt_clone", "zal_rt_drop", "zal_rt_len", "zal_rt_index", "zal_rt_for_next", "zal_rt_upd", "zal_rt_tag", "zal_rt_payload", "zal_rt_field", "zal_rt_map_get", "zal_rt_const", "zal_rt_concat", "zal_rt_make", "zal_rt_builtin", "zal_rt_map_get_jump"] {
        assert!(names.contains(&n), "{n} missing from runtime_symbols()");
    }
}

/// v3 helpers need a running context: a program whose every aggregate,
/// builtin and map lookup goes through them (fake_body) must equal the
/// interpreter, including the cases that fall back (traps, Print).
#[test]
fn v3_helpers_in_a_running_program() {
    let src = r#"module t
type P = { a: Int, b: Str }
enum E { A, B(Int) }
fn g(e: E) -> Int { match e { E.A => 0, E.B(k) => k } }
fn f(n: Int) -> Str {
  let mut xs: List<Int> = []
  for i in range(n) { xs = push(xs, i * 3) }
  xs = set(xs, 0, 7)
  xs = remove(xs, 1)
  let p = P { a: len(xs), b: "q" }
  let t = (p.a, p.b)
  let mut m: Map<Int> = map_with_capacity(4)
  m = map_set(m, "x", 1)
  m = map_set(m, "y", 2)
  let mut hit = 0
  match map_get(m, "x") { Some(v) => { hit = hit + v }, None => {} }
  match map_get(m, "z") { Some(v) => { hit = hit + v }, None => { hit = hit + 100 } }
  let ks = map_keys(m)
  let j = json_str("a\"b")
  let o = match get(xs, 2) { Some(v) => v, None => 0 }
  print(str(t.0) + t.1)
  str(len(xs)) + "/" + str(hit) + "/" + str(len(ks)) + "/" + j + "/" + str(o) + "/" + str(g(E.B(5)) + g(E.A)) + "/" + str(map_has(m, "y"))
}"#;
    let p = zal_compiler::compile_source(src, "t.zal").unwrap();
    let mut outs = vec![];
    for table in [None, Some(fake::table(&p)), Some(fake::table_generic(&p))] {
        let mut vm = Vm::new(&p, MockHost::default(), PolicyBroker::new(vec![]));
        if let Some(t) = table {
            vm.set_native(t);
        }
        let r = vm.run("f", vec![Value::Int(20)]).map(|v| v.to_string()).map_err(|e| e.to_string());
        outs.push((r, vm.output.clone()));
    }
    assert!(outs[0].0.is_ok(), "{:?}", outs[0]);
    assert_eq!(outs[0], outs[1]);
    assert_eq!(outs[0], outs[2]);
    // Out-of-range set traps identically through the helper's fallback.
    let src = "module t\nfn f() -> Int { let mut xs: List<Int> = [1]\n xs = set(xs, 5, 1)\n len(xs) }";
    let p = zal_compiler::compile_source(src, "t.zal").unwrap();
    let mut errs = vec![];
    for table in [None, Some(fake::table(&p))] {
        let mut vm = Vm::new(&p, MockHost::default(), PolicyBroker::new(vec![]));
        if let Some(t) = table {
            vm.set_native(t);
        }
        errs.push(vm.run("f", vec![]).unwrap_err().to_string());
    }
    assert_eq!(errs[0], errs[1]);
}

#[test]
fn make_and_map_get_jump_on_a_raw_stack() {
    // Drive the helpers through a native body on a tiny program, comparing the
    // stack effect with the interpreter's ops via whole-program equality above;
    // here only the encodings: kind codes and the variant packing.
    assert_eq!((ZAL_MAKE_LIST, ZAL_MAKE_TUPLE, ZAL_MAKE_RECORD, ZAL_MAKE_VARIANT), (0, 1, 2, 3));
}
